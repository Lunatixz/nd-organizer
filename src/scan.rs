// Full-library scan: a chunked, resumable walk that builds a metadata index,
// then groups files into albums by their TAGS (not folders) and plans/applies
// the result. Only available on the wasm target (uses host services).

use std::path::Path;

use nd_pdk::host;
use serde_json::{json, Value};

use crate::config::{Config, Mode};
use crate::organizer::is_audio;
use crate::state::{cap_ms, past, remain_ms, since};
use crate::tags::TrackTags;

fn lib_root(library_id: i32) -> Result<std::path::PathBuf, String> {
    let lib = host::library::get_library(library_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("library {library_id} not found"))?;
    if lib.mount_point.is_empty() {
        return Err("library has no filesystem mount".into());
    }
    Ok(Path::new(&lib.mount_point).to_path_buf())
}

/// The library's REAL path as Navidrome sees it (e.g. /music, /unsorted). The
/// AcoustID sidecar must mount the library at this same path, so the plugin
/// sends `{path}/{rel}` when asking it to fingerprint a file.
fn library_real_path(library_id: i32) -> Result<String, String> {
    let lib = host::library::get_library(library_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("library {library_id} not found"))?;
    if lib.path.is_empty() {
        return Err(format!("library {library_id} has no path configured"));
    }
    Ok(lib.path.trim_end_matches('/').to_string())
}

fn stack_key(library_id: i32) -> String {
    format!("scan.stackv2.{library_id}")
}
fn file_key(library_id: i32, rel: &str) -> String {
    crate::state::file_index_key(library_id, rel)
}

/// Unwrap the index value format `{"tags": {...}}` written by index_step.
/// plan_singles used to parse the wrapper as TrackTags directly — every
/// parse failed and the whole Singles queue was consumed with 0 moves.
fn parse_file_tags(v: &[u8]) -> Option<TrackTags> {
    let val: serde_json::Value = serde_json::from_slice(v).ok()?;
    let tags = val.get("tags")?.clone();
    if tags.is_null() {
        return None;
    }
    serde_json::from_value::<TrackTags>(tags).ok()
}

fn load_stack(key: &str) -> Vec<String> {
    match crate::store::kv().get(key) {
        Ok(Some(v)) => serde_json::from_slice(&v).unwrap_or_else(|_| vec![String::new()]),
        _ => vec![String::new()],
    }
}

fn file_mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read + index one file's tags (skips unchanged files via mtime cache).
/// Returns `true` if the file was actually (re)indexed, `false` if skipped.
fn index_file(cfg: &Config, library_id: i32, rel: &str, abs: &Path) -> Result<bool, String> {
    let mtime = file_mtime(abs);
    let key = file_key(library_id, rel);
    if let Ok(Some(v)) = crate::store::kv().get(&key) {
        if let Ok(val) = serde_json::from_slice::<Value>(&v) {
            if val.get("mtime").and_then(|m| m.as_i64()) == Some(mtime) {
                return Ok(false);
            }
        }
    }
    let mut tags = crate::tags::read_tags(abs);
    // Tagless / empty-tagged files: fall back to parsing the filename so they
    // still participate in organization (gated by parseFilenames).
    let usable = tags
        .as_ref()
        .map(|t| !t.title.trim().is_empty() || !t.artist.trim().is_empty())
        .unwrap_or(false);
    if !usable && cfg.parse_filenames {
        tags = Some(crate::organizer::tags_from_filename(rel));
    }
    let entry = match tags {
        Some(t) => json!({ "rel": rel, "tags": t, "mtime": mtime }),
        None => json!({ "rel": rel, "tags": null, "mtime": mtime }),
    };
    crate::store::kv().set(&key, entry.to_string().into_bytes()).map_err(|e| e.to_string())?;
    Ok(true)
}

/// How a scan chunk ended: `More` = resume next task, `Done` = the walk
/// finished, `Paused` = hit the per-pass `maxScanEntries` cap (resumes next run).
pub enum ScanOutcome {
    More,
    Done,
    Paused,
}

/// Scan the next chunk of the library. Returns `(outcome, files_indexed)`.
pub fn scan_step(cfg: &Config, library_id: i32) -> Result<(ScanOutcome, usize), String> {
    let root = lib_root(library_id)?;
    let key = stack_key(library_id);
    let mut stack = load_stack(&key);
    let files_per_task = cfg.files_per_scan_task.max(1);
    // Cap directories walked per task to avoid the 30s WASM deadline on fresh
    // rescans where the directory tree is large. Tag indexing is already capped
    // by files_per_task; this caps the directory traversal overhead.
    // ponytail: hardcoded limit, add config field if users need to tune it.
    let dirs_per_task: usize = 50;
    let mut dirs_walked: usize = 0;
    // Per-pass cumulative cap (maxScanEntries; 0 = unlimited). run_pass resets
    // this counter, so the cap throttles how much one pass indexes; the saved
    // stack resumes where it stopped on the next run.
    let pass_count = crate::store::kv()
        .get(&format!("scan.pass.{library_id}"))
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8_lossy(&v).parse::<usize>().ok())
        .unwrap_or(0);
    let cap = cfg.max_scan_entries;
    let mut processed = 0usize;
    let mut skipped = 0usize;
    let mut hit_limit = false;
    let mut last_rel: String = String::new();
    let _initial_stack = stack.len();
    // Hard time cap: break out before the WASM deadline regardless of
    // dir/file counters. Check every 200 dirs to avoid syscall overhead.
    let scan_start = std::time::SystemTime::now();
    let time_budget = std::time::Duration::from_secs(15);
    let mut dirs_since_check: usize = 0;

    crate::wasm::log_info(&format!(
        "scan_step: starting chunk, stack={} files, pass_count={}",
        stack.len(), pass_count
    ));

    while let Some(dir_rel) = stack.pop() {
        if crate::organizer::is_excluded(&dir_rel, &cfg.exclude_paths) {
            continue;
        }
        // Time-based break: bail before the 30s WASM deadline.
        dirs_since_check += 1;
        if dirs_since_check >= 200 {
            dirs_since_check = 0;
            if past(scan_start, time_budget) {
                stack.push(dir_rel);
                hit_limit = true;
                break;
            }
        }
        let dir_path = root.join(&dir_rel);
        let Ok(entries) = std::fs::read_dir(&dir_path) else {
            continue;
        };
        let mut subdirs: Vec<String> = Vec::new();
        let dir_start_processed = processed;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if cfg.skip_hidden_files && name.starts_with('.') {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let rel = if dir_rel.is_empty() {
                name.clone()
            } else {
                format!("{dir_rel}/{name}")
            };
            if ft.is_dir() {
                subdirs.push(rel);
            } else if ft.is_file() && is_audio(&name) {
                if cap > 0 && pass_count + processed >= cap {
                    hit_limit = true;
                    break;
                }
                last_rel = rel.clone();
                let did_work = index_file(cfg, library_id, &rel, &entry.path())?;
                if did_work {
                    processed += 1;
                } else {
                    skipped += 1;
                }
                if processed >= files_per_task {
                    hit_limit = true;
                    break;
                }
            }
        }
        for sub in subdirs.into_iter().rev() {
            stack.push(sub);
        }
        // Only count directories that actually indexed new files toward the
        // budget. Fully-indexed dirs are free — the scan walks them once to
        // confirm, then skips them on subsequent chunks via mtime checks.
        if processed > dir_start_processed {
            dirs_walked += 1;
        }
        if dirs_walked >= dirs_per_task {
            // Save remaining stack and re-enqueue to stay under 30s.
            stack.push(dir_rel);
            hit_limit = true;
            break;
        }
        if hit_limit {
            // Resume this dir next time (already-indexed files are skipped).
            stack.push(dir_rel);
            break;
        }
    }

    // Record the per-pass count so the cap is cumulative across chunks.
    let _ = crate::store::kv().set(
        &format!("scan.pass.{library_id}"),
        (pass_count + processed).to_string().into_bytes(),
    );
    let capped = cap > 0 && pass_count + processed >= cap && hit_limit;
    crate::wasm::log_info(&format!(
        "scan_step: chunk done, processed={}, skipped={}, stack_now={}, hit_limit={}",
        processed, skipped, stack.len(), hit_limit
    ));

    if capped {
        crate::store::kv().set(&key, serde_json::to_vec(&stack).unwrap_or_default())
            .map_err(|e| e.to_string())?;
        post_scan_status(cfg, library_id, processed, &last_rel);
        Ok((ScanOutcome::Paused, processed))
    } else if hit_limit {
        crate::store::kv().set(&key, serde_json::to_vec(&stack).unwrap_or_default())
            .map_err(|e| e.to_string())?;
        crate::wasm::enqueue_scan_task(library_id)?;
        post_scan_status(cfg, library_id, processed, &last_rel);
        Ok((ScanOutcome::More, processed))
    } else {
        let _ = crate::store::kv().delete(&key);
        let _ = crate::store::kv().set(&format!("scan.donev2.{library_id}"), b"1".to_vec());
        crate::wasm::enqueue_group_task(library_id)?;
        post_scan_status(cfg, library_id, processed, &last_rel);
        Ok((ScanOutcome::Done, processed))
    }
}

/// Push a scan-progress status to the webhook dashboard after every chunk so
/// the user sees activity during the (slow) library scan, not just at the end.
fn post_scan_status(cfg: &Config, library_id: i32, chunk: usize, current_file: &str) {
    let total = {
        let key = format!("scan.count.{library_id}");
        let cur: i64 = crate::store::kv().get(&key)
            .ok()
            .flatten()
            .and_then(|v| String::from_utf8_lossy(&v).parse().ok())
            .unwrap_or(0);
        let new = cur + chunk as i64;
        let _ = crate::store::kv().set(&key, new.to_string().into_bytes());
        new
    };

    // Calculate ETA if we have a known total from phase-specific KV keys.
    let (known_total, phase_name) = if let Some(v) = crate::store::kv()
        .get(&format!("scan.verify_total.{library_id}"))
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8(v).ok())
        .and_then(|s| s.parse::<i64>().ok())
    {
        (Some(v), "verify")
    } else if let Some(v) = crate::store::kv()
        .get(&format!("scan.index_total.{library_id}"))
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8(v).ok())
        .and_then(|s| s.parse::<i64>().ok())
    {
        (Some(v), "index")
    } else {
        (None, "scan")
    };

    let eta = if let Some(tot) = known_total {
        if total > 0 && tot > total {
            let start_key = format!("scan.phase_start.{}", library_id);
            let start_ts: i64 = crate::store::kv()
                .get(&start_key)
                .ok()
                .flatten()
                .and_then(|v| String::from_utf8(v).ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| {
                    let now = crate::state::now_ts();
                    let _ = crate::store::kv().set(&start_key, now.to_string().into_bytes());
                    now
                });
            let elapsed = crate::state::now_ts() - start_ts;
            if elapsed > 0 {
                let rate = total as f64 / elapsed as f64;
                Some(((tot - total) as f64 / rate) as u64)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let status = serde_json::json!({
        "ts": crate::state::now_ts(),
        "mode": crate::wasm::mode_label(cfg),
        "inProgress": true,
        "phase": phase_name,
        "filesScanned": total,
        "chunkSize": chunk,
        "currentFile": current_file,
        "etaSeconds": eta,
        "libraries": [{
            "id": library_id,
            "albumsFound": 0,
            "albumsToMove": 0,
            "fileMoves": 0,
            "kept": 0,
            "skipped": 0,
            "duplicates": 0,
            "filesScanned": total
        }],
        "warnings": [],
        "integrations": crate::wasm::integration_health(cfg),
        "tasks": crate::wasm::task_log(),
    })
    .to_string();
    crate::wasm::post_webhook(cfg, &status);
}

/// Post a lightweight phase-only status so the dashboard pipeline stepper
/// highlights the current phase (group, plan, etc.) during processing.
/// Optionally includes ETA if current/total are provided.
fn post_phase_status(cfg: &Config, library_id: i32, phase: &str) {
    let status = serde_json::json!({
        "ts": crate::state::now_ts(),
        "mode": crate::wasm::mode_label(cfg),
        "inProgress": true,
        "phase": phase,
        "phaseDetail": match phase {
            "group" => "Loading indexed files and reading tags from KV store...",
            "plan" => "Moving files to organized folders and recording rollback...",
            "enrich" => "Running metadata enrichment (artwork, lyrics, genre, etc.)...",
            "cleanup" => "Removing empty no-audio folders...",
            _ => "",
        },
        "libraries": [{
            "id": library_id,
            "albumsFound": 0,
            "albumsToMove": 0,
            "fileMoves": 0,
            "kept": 0,
            "skipped": 0,
            "duplicates": 0,
        }],
        "warnings": [],
        "integrations": crate::wasm::integration_health(cfg),
        "tasks": crate::wasm::task_log(),
    })
    .to_string();
    crate::wasm::post_webhook(cfg, &status);
}

/// Post scan status with ETA calculation.
/// `current` = items processed so far, `total` = total items (0 if unknown).
fn post_scan_status_with_eta(cfg: &Config, library_id: i32, phase: &str, current: usize, total: usize, current_file: &str) {
    // Calculate ETA based on processing rate.
    let eta_seconds = if current > 0 && total > current {
        // Load start time for this phase from KV.
        let start_key = format!("scan.phase_start.{}.{}", library_id, phase);
        let start_ts: i64 = crate::store::kv()
            .get(&start_key)
            .ok()
            .flatten()
            .and_then(|v| String::from_utf8(v).ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                // First call — record start time.
                let now = crate::state::now_ts();
                let _ = crate::store::kv().set(&start_key, now.to_string().into_bytes());
                now
            });
            let elapsed = crate::state::now_ts() - start_ts;
            if elapsed > 0 {
                let rate = current as f64 / elapsed as f64;
                let remaining = (total - current) as f64 / rate;
                Some(remaining as u64)
            } else {
                None
            }
        } else {
            None
        };

        let status = serde_json::json!({
            "ts": crate::state::now_ts(),
            "mode": crate::wasm::mode_label(cfg),
            "inProgress": true,
            "phase": phase,
            "phaseDetail": format!("{}/{} files {}", current, total, current_file),
            "progress": {
                "current": current,
                "total": total,
                "percent": if total > 0 { (current as f64 / total as f64 * 100.0) as u64 } else { 0 },
                "etaSeconds": eta_seconds,
            },
            "libraries": [{
                "id": library_id,
                "filesScanned": current,
            }],
            "warnings": [],
            "integrations": crate::wasm::integration_health(cfg),
            "tasks": crate::wasm::task_log(),
        })
        .to_string();
        crate::wasm::post_webhook(cfg, &status);
    }

// ---------------------------------------------------------------------------
// Snapshot scan: two-phase walk → index
// ---------------------------------------------------------------------------

fn walk_stack_key(library_id: i32) -> String {
    format!("scan.walkv2.{library_id}")
}
fn walk_files_key(library_id: i32) -> String {
    format!("scan.walkfiles.{library_id}")
}

/// Phase 1: Walk the directory tree and collect audio file paths + mtimes.
/// No tag reading — just filesystem traversal. Each chunk walks until the
/// time budget expires, saves the accumulated list, and re-enqueues.
/// When the tree is fully walked, transitions to Phase 2 (index_step).
pub fn walk_step(
    cfg: &Config,
    library_id: i32,
) -> Result<ScanOutcome, String> {
    let root = lib_root(library_id)?;
    let key = walk_stack_key(library_id);
    let mut stack = load_stack(&key);

    // Delta approach: don't load the full accumulated file list (too expensive
    // for 30K+ entries). Instead, accumulate new files in memory and save to a
    // delta key. When walk completes, merge delta into the main file list.
    let files_key = walk_files_key(library_id);
    let delta_key = format!("scan.walkdelta.{library_id}");
    let mut files: Vec<(String, i64)> = Vec::new();

    // Only clear stale state on first chunk (when delta doesn't exist yet).
    if crate::store::kv().get(&delta_key).ok().flatten().is_none() {
        let _ = crate::store::kv().delete(&files_key);
        let _ = crate::store::kv().delete(&format!("scan.walkcount.{library_id}"));
    }

    // Load the current file count for the log message (avoid full deserialization).
    let file_count: usize = crate::store::kv()
        .get(&format!("scan.walkcount.{library_id}"))
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // Track visited directories to avoid re-walking the same dirs.
    let dirs_key = format!("scan.walkdirs.{library_id}");
    let mut visited_dirs: std::collections::HashSet<String> = crate::store::kv()
        .get(&dirs_key)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or_default();

    // Dedup stack against visited_dirs — removes dead entries from previous chunks.
    stack.retain(|d| !visited_dirs.contains(d));

    // Track seen files to avoid counting duplicates (symlinks, hardlinks).
    let mut seen_files: std::collections::HashSet<String> = std::collections::HashSet::new();

    let scan_start = std::time::SystemTime::now();
    let time_budget = std::time::Duration::from_secs(15);
    let mut dirs_since_check: usize = 0;
    let mut dirs_walked: usize = 0;
    let mut entries_since_check: usize = 0;
    // Cap entries per chunk to avoid one huge directory consuming the entire
    // 30s budget. Time check runs every 200 entries to avoid syscall overhead.
    let entries_per_chunk: usize = 500;
    let mut hit_limit = false;

    crate::wasm::log_info(&format!(
        "walk_step: starting chunk, stack={}, files_so_far={}",
        stack.len(), file_count + files.len()
    ));

    while let Some(dir_rel) = stack.pop() {
        if crate::organizer::is_excluded(&dir_rel, &cfg.exclude_paths) {
            continue;
        }
        // Skip directories already fully walked in previous chunks.
        if !visited_dirs.insert(dir_rel.clone()) {
            continue;
        }
        dirs_since_check += 1;
        if dirs_since_check >= 10 {
            dirs_since_check = 0;
            if past(scan_start, time_budget) {
                stack.push(dir_rel);
                break;
            }
        }
        let dir_path = root.join(&dir_rel);
        let Ok(entries) = std::fs::read_dir(&dir_path) else {
            continue;
        };
        let mut subdirs: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if cfg.skip_hidden_files && name.starts_with('.') {
                continue;
            }
            let rel = if dir_rel.is_empty() {
                name.clone()
            } else {
                format!("{dir_rel}/{name}")
            };
            // Check extension first to skip stat calls on non-audio files.
            if is_audio(&name) {
                if let Ok(ft) = entry.file_type() {
                    if ft.is_file() {
                        let mtime = file_mtime(&entry.path());
                        if seen_files.insert(rel.clone()) {
                            files.push((rel, mtime));
                        }
                    }
                }
            } else if let Ok(ft) = entry.file_type() {
                if ft.is_dir() {
                    subdirs.push(rel);
                }
            }
            entries_since_check += 1;
            if entries_since_check >= 200 {
                entries_since_check = 0;
                if past(scan_start, time_budget) || files.len() >= entries_per_chunk {
                    hit_limit = true;
                    break;
                }
            }
        }
        for sub in subdirs.into_iter().rev() {
            // Only push subdirs not already visited or queued.
            // Use visited_dirs for O(1) lookup instead of stack.contains() which is O(n).
            if !visited_dirs.contains(&sub) {
                stack.push(sub);
            }
        }
        dirs_walked += 1;
        if hit_limit {
            break;
        }
    }

    crate::wasm::log_info(&format!(
        "walk_step: chunk done, dirs_walked={}, new_files={}, stack_remaining={}",
        dirs_walked, files.len(), stack.len()
    ));

    let total_files = file_count + files.len();

    if stack.is_empty() {
        // Tree fully walked — merge delta into main file list and transition.
        let _ = crate::store::kv().delete(&key);
        let _ = crate::store::kv().delete(&dirs_key);
        // Load accumulated delta from previous chunks, then clean it up.
        let mut all_files: Vec<(String, i64)> = crate::store::kv()
            .get(&delta_key)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default();
        let _ = crate::store::kv().delete(&delta_key);
        // Append files from this final chunk.
        all_files.extend(files);
        crate::store::kv()
            .set(&files_key, serde_json::to_vec(&all_files).unwrap_or_default())
            .map_err(|e| e.to_string())?;
        // Save file count for next walk's log message.
        let _ = crate::store::kv().set(&format!("scan.walkcount.{library_id}"), all_files.len().to_string().into_bytes());
        crate::wasm::enqueue_index_task(library_id)?;
        post_scan_status(cfg, library_id, 0, &format!("walk complete: {} files found", all_files.len()));
        Ok(ScanOutcome::Done)
    } else {
        // More directories to walk — save delta + state and re-enqueue.
        crate::store::kv()
            .set(&key, serde_json::to_vec(&stack).unwrap_or_default())
            .map_err(|e| e.to_string())?;
        // Save only new files from this chunk (delta), not the full list.
        if !files.is_empty() {
            // Append delta to existing delta key.
            let mut delta: Vec<(String, i64)> = crate::store::kv()
                .get(&delta_key)
                .ok()
                .flatten()
                .and_then(|v| serde_json::from_slice(&v).ok())
                .unwrap_or_default();
            delta.extend(files);
            crate::store::kv()
                .set(&delta_key, serde_json::to_vec(&delta).unwrap_or_default())
                .map_err(|e| e.to_string())?;
            // Update file count for next chunk's log message.
            let _ = crate::store::kv().set(&format!("scan.walkcount.{library_id}"), delta.len().to_string().into_bytes());
        }
        crate::store::kv()
            .set(&dirs_key, serde_json::to_vec(&visited_dirs).unwrap_or_default())
            .map_err(|e| e.to_string())?;
        crate::wasm::enqueue_walk_task(library_id)?;
        post_scan_status(cfg, library_id, 0, &format!(
            "walking... {} files found, {} directories remaining",
            total_files, stack.len()
        ));
        Ok(ScanOutcome::More)
    }
}

/// Phase 2: Index files from the snapshot list collected by walk_step.
/// Reads tags for each file, stores in KV for group_step. Chunked by
/// files_per_task to stay under the 30s WASM deadline.
pub fn index_step(
    cfg: &Config,
    library_id: i32,
) -> Result<(ScanOutcome, usize), String> {
    let root = lib_root(library_id)?;
    let files_key = walk_files_key(library_id);
    let cursor_key = format!("scan.index_cursor.{library_id}");
    let indexed_key = format!("scan.indexed.{library_id}");
    let paths_key = format!("scan.group_paths.{library_id}");
    // ponytail: propagate read errors — during a mysql outage the fallback
    // store misses, and treating that as "no files" silently completed the
    // index phase (twice: deleted cursor + enqueued group with 0 files).
    let files: Vec<(String, i64)> = crate::store::kv()
        .get(&files_key)
        .map_err(|e| format!("index_step: files_key read failed: {e}"))?
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or_default();

    if files.is_empty() {
        // Nothing to index — transition to group phase.
        let _ = crate::store::kv().delete(&files_key);
        let _ = crate::store::kv().delete(&cursor_key);
        let _ = crate::store::kv().set(&format!("scan.donev2.{library_id}"), b"1".to_vec());
        crate::wasm::enqueue_group_task(library_id)?;
        post_scan_status(cfg, library_id, 0, "index complete");
        return Ok((ScanOutcome::Done, 0));
    }

    // Resume from cursor — skip files already checked in previous chunks.
    let mut i: usize = crate::store::kv()
        .get(&cursor_key)
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if i > files.len() {
        i = 0;
    }

    // Both list keys hold walk_files content, which is frozen for the whole
    // index phase (walk completes first; init/force-rescan clear both keys),
    // so after the first chunk of a pass they never change. Re-serializing
    // them every chunk costs ~10s of wasm CPU + two multi-MB KV writes —
    // the tail that was killing tasks at the 30s deadline mid-pass.
    let have_lists = crate::store::kv().get(&indexed_key).ok().flatten().is_some()
        && crate::store::kv().get(&paths_key).ok().flatten().is_some();

    let files_per_task = cfg.files_per_scan_task.max(1);
    let cap = cfg.max_scan_entries;
    let pass_count: usize = crate::store::kv()
        .get(&format!("scan.pass.{library_id}"))
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8_lossy(&v).parse().ok())
        .unwrap_or(0);

    let scan_start = std::time::SystemTime::now();
    // 15s budget — Navidrome's WASM scheduler kills at ~27s.
    // ponytail: the WASM clock accrues while running, not while blocked in
    // host calls (kv HTTP, mount stats) — on IO-heavy chunks the budget
    // starves and the task gets killed at 30s instead. iters_per_task counts
    // ALL iterations (skips included) so every chunk ends predictably.
    let time_budget = std::time::Duration::from_secs(15);
    let start_i = i;
    // ponytail: count break must be SMALL — a cold index_file costs ~360ms
    // (kv get + lofty tag read + kv set, each a WASM→sidecar HTTP hop) and
    // elapsed() does NOT accrue host-call time, so the time budget never
    // fires. 30 iters ≈ 11s typical / ~21s worst, under the 30s kill.
    // filesPerScanTask drives the count; 30 is the deadline-safe ceiling —
    // a cold 200-file chunk would run ~72s and get killed mid-task.
    let iters_per_task: usize = files_per_task.min(30);
    let mut processed = 0usize;
    let mut skipped = 0usize;
    let mut last_rel = String::new();

    crate::wasm::log_info(&format!(
        "index_step: starting chunk, cursor={}, files_total={}, pass_count={}",
        i, files.len(), pass_count
    ));

    // Process files from cursor position.
    // Skip files whose mtime hasn't changed (already indexed).
    while i < files.len() {
        if i - start_i >= iters_per_task {
            break;
        }
        if past(scan_start, time_budget) {
            break;
        }
        if cap > 0 && pass_count + processed >= cap {
            break;
        }
        if processed >= files_per_task {
            break;
        }
        let (rel, _) = &files[i];
        last_rel = rel.clone();
        let abs = root.join(rel);
        // ponytail: no walk-mtime shortcut here — it compared against the
        // walk-snapshot (always equal on a fresh pass) and skipped EVERY file
        // before index_file could write its kv entry, leaving group_step with
        // zero entries. index_file's own kv+mtime check is the real gate.
        let did_work = index_file(cfg, library_id, rel, &abs)?;
        if did_work {
            processed += 1;
        } else {
            skipped += 1;
        }
        i += 1;
        if past(scan_start, time_budget) {
            break;
        }
    }

    let finished = i >= files.len();
    let capped = cap > 0 && pass_count + processed >= cap;
    let pass_key = format!("scan.pass.{library_id}");

    crate::wasm::log_info(&format!(
        "index_step: chunk done, processed={}, skipped={}, cursor={}/{}, pass={}",
        processed, skipped, i, files.len(), pass_count + processed
    ));

    if !finished || capped {
        // Not done yet — save cursor for resume and re-enqueue.
        let _ = crate::store::kv().set(&cursor_key, i.to_string().into_bytes());
        let _ = crate::store::kv().set(
            &pass_key,
            (pass_count + processed).to_string().into_bytes(),
        );
        // Save incremental progress to indexed key so group_step can read it
        // even if the final completion times out (40K serialization can exceed WASM budget).
        // Skipped when both keys already exist this pass — content unchanged.
        if !have_lists {
            let _ = crate::store::kv().set(&indexed_key, serde_json::to_vec(&files).unwrap_or_default());
            // Also save paths-only for group_step.
            let paths: Vec<String> = files.iter().map(|(rel, _)| rel.clone()).collect();
            if let Err(e) = crate::store::kv().set(&paths_key, serde_json::to_vec(&paths).unwrap_or_default()) {
                crate::wasm::log_warn(&format!("index_step: failed to write paths_key (incremental): {e}"));
            }
        }
        post_scan_status(cfg, library_id, processed, &last_rel);
        crate::wasm::enqueue_index_task(library_id)?;
        Ok((ScanOutcome::Paused, processed))
    } else {
        // All files done — save full list to indexed key for group_step.
        // Delete walk_files FIRST to free KV storage before writing indexed+unverified.
        // This prevents temporarily doubling storage (which can hit the 100MB KV limit
        // and cause WAL bloat that blocks plugin reload after crashes).
        let _ = crate::store::kv().delete(&files_key);
        // Rewrite the list keys only if the incremental copy never landed —
        // with files_key gone, have_lists is what group_step falls back on
        // when this completion call dies mid-write.
        if !have_lists {
            let files_bytes = serde_json::to_vec(&files).unwrap_or_default();
            crate::store::kv()
                .set(&indexed_key, files_bytes)
                .map_err(|e| e.to_string())?;
            // Store file paths only (no mtimes) for group_step to read quickly.
            // The full indexed_key (with mtimes) is too slow to deserialize in WASM.
            let paths: Vec<String> = files.iter().map(|(rel, _)| rel.clone()).collect();
            let paths_bytes = serde_json::to_vec(&paths).unwrap_or_default();
            crate::wasm::log_info(&format!(
                "index_step: writing paths_key={}, size={} bytes",
                paths_key, paths_bytes.len()
            ));
            if let Err(e) = crate::store::kv().set(&paths_key, paths_bytes) {
                crate::wasm::log_warn(&format!("index_step: failed to write paths_key: {e}"));
            }
        }
        // Delete cursor AFTER paths_key is written — group checks cursor to defer.
        let _ = crate::store::kv().delete(&cursor_key);
        // Pre-cache the unverified list as paths only (no mtimes) so verify_step
        // doesn't need to recompute from the full indexed key (which times out WASM).
        let unverified_key = format!("scan.unverified.{library_id}");
        let unverified_paths: Vec<String> = files.iter().map(|(rel, _)| rel.clone()).collect();
        let _ = crate::store::kv().set(&unverified_key, serde_json::to_vec(&unverified_paths).unwrap_or_default());
        let _ = crate::store::kv().set(
            &pass_key,
            (pass_count + processed).to_string().into_bytes(),
        );
        post_scan_status(cfg, library_id, processed, &format!(
            "indexing complete: {} files indexed, {} skipped (unchanged)",
            processed, skipped
        ));
        crate::wasm::enqueue_verify_task(library_id)?;
        post_scan_status(cfg, library_id, processed, &last_rel);
        Ok((ScanOutcome::Done, processed))
    }
}

/// Phase 3: Verify file identities via AcoustID sidecar batch processing.
/// Loads unverified files, sends them to the acoustid sidecar in batches,
/// saves verified results to KV, and enqueues group task when complete.

/// Load or recompute the unverified file list. Cached in KV after first compute.
/// The entire recompute is budgeted to 20s to stay within the WASM 30s deadline.
fn load_unverified(
    cfg: &Config,
    library_id: i32,
    indexed_key: &str,
    unverified_key: &str,
) -> Vec<(String, i64)> {
    // Try to load from unverified key. Handle both formats:
    // - New: Vec<String> (paths only, written by index)
    // - Old: Vec<(String, i64)> (paths + mtimes, from previous runs)
    crate::store::kv()
        .get(unverified_key)
        .ok()
        .flatten()
        .and_then(|v| {
            // Try new paths-only format first.
            if let Ok(paths) = serde_json::from_slice::<Vec<String>>(&v) {
                return Some(paths.into_iter().map(|p| (p, 0i64)).collect());
            }
            // Fall back to old format.
            serde_json::from_slice(&v).ok()
        })
        .unwrap_or_else(|| {
            crate::wasm::log_info("verify_step: recomputing unverified list from indexed key");
            let recompute_start = std::time::SystemTime::now();
            let recompute_budget = std::time::Duration::from_secs(20);

            let file_list: Vec<(String, i64)> = match crate::store::kv()
                .get(indexed_key)
                .ok()
                .flatten()
                .and_then(|v| serde_json::from_slice(&v).ok())
            {
                Some(list) => list,
                None => return vec![],
            };

            if past(recompute_start, recompute_budget) {
                crate::wasm::log_warn("verify_step: recompute budget exceeded after loading indexed list");
                return vec![];
            }

            let file_keys: Vec<String> = file_list.iter().map(|(rel, _)| file_key(library_id, rel)).collect();
            let batch_size_kv = 500;
            let mut verified_set: std::collections::HashSet<String> = std::collections::HashSet::new();
            for chunk in file_keys.chunks(batch_size_kv) {
                if past(recompute_start, recompute_budget) {
                    break;
                }
                if let Ok(entries) = crate::store::kv().get_many(chunk.to_vec()) {
                    for (k, v) in entries {
                        if let Ok(val) = serde_json::from_slice::<Value>(&v) {
                            if let Some(tags) = val.get("tags") {
                                if !tags.is_null() {
                                    // Re-fingerprint when either artist or
                                    // album_artist has no meta or contains
                                    // "Unknown" (missing tag counts as no meta).
                                    let force_unknown = cfg.force_refingerprint_unknown_artist
                                        && (tags.get("artist").and_then(|a| a.as_str()).map(crate::tags::is_unknown_artist).unwrap_or(true)
                                            || tags.get("album_artist").and_then(|a| a.as_str()).map(crate::tags::is_unknown_artist).unwrap_or(true));

                                    if !force_unknown && !cfg.force_fingerprint {
                                        if let Ok(t) = serde_json::from_value::<TrackTags>(tags.clone()) {
                                            if !t.mbid_album.is_empty() {
                                                verified_set.insert(k);
                                                continue;
                                            }
                                        }
                                    }
                                    if !force_unknown && tags.get("_acoustid_checked").and_then(|v| v.as_bool()).unwrap_or(false) {
                                        verified_set.insert(k);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let list: Vec<(String, i64)> = file_list.iter().filter(|(rel, _)| {
                !verified_set.contains(&file_key(library_id, rel))
            }).cloned().collect();
            crate::wasm::log_info(&format!(
                "verify_step: recompute done in {:?}, {} unverified of {} total (checked {}/{})",
                since(recompute_start), list.len(), file_list.len(),
                verified_set.len(), file_keys.len()
            ));
            let _ = crate::store::kv().set(unverified_key, serde_json::to_vec(&list).unwrap_or_default());
            list
        })
}
pub fn verify_step(
    cfg: &Config,
    library_id: i32,
) -> Result<(ScanOutcome, usize), String> {
    let root = lib_root(library_id)?;
    post_phase_status(cfg, library_id, "verify");

    let indexed_key = format!("scan.indexed.{library_id}");
    let unverified_key = format!("scan.unverified.{library_id}");
    let job_id_key = format!("scan.verify_job.{library_id}");

    // Async verify: send batches to AcoustID sidecar, poll for results.
    let acoustid_url = cfg.acoustid_url.trim().trim_end_matches('/');
    let job_id = format!("verify-{}-{}", library_id, crate::wasm::current_run_id(library_id).unwrap_or_default());

    // Check for existing job FIRST — this is the fast path (most verify calls
    // just poll). Loading the 40K-entry unverified list is deferred until we
    // actually need to send batches or process completed results.
    let existing_job_id = crate::store::kv()
        .get(&job_id_key)
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8(v).ok());

    if let Some(ref existing_id) = existing_job_id {
        // Poll job status
        let status_url = format!("{}/job-status?job_id={}", acoustid_url, existing_id);
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url: status_url,
            headers: std::collections::HashMap::new(),
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 5_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                crate::net::circuit_clear("acoustid");
                let status: serde_json::Value = serde_json::from_slice(&resp.body)
                    .map_err(|e| format!("bad status response: {e}"))?;
                let processing = status.get("processing").and_then(|p| p.as_bool()).unwrap_or(false);
                let done = status.get("done").and_then(|d| d.as_u64()).unwrap_or(0) as usize;
                let total = status.get("total").and_then(|t| t.as_u64()).unwrap_or(0) as usize;

                if processing {
                    crate::wasm::log_info(&format!(
                        "verify_step: job {} still processing ({}/{} done)",
                        existing_id, done, total
                    ));
                    crate::wasm::enqueue_verify_task(library_id)?;
                    post_scan_status(cfg, library_id, done, &format!(
                        "verifying... {}/{} files", done, total
                    ));
                    return Ok((ScanOutcome::More, done));
                }

                // Job completed — load unverified list and process results
                let unverified = load_unverified(cfg, library_id, &indexed_key, &unverified_key);
                let results_url = format!("{}/job-results?job_id={}", acoustid_url, existing_id);
                let req = host::http::HTTPRequest {
                    method: "GET".into(),
                    url: results_url,
                    headers: std::collections::HashMap::new(),
                    no_follow_redirects: false,
                    body: vec![],
                    timeout_ms: 10_000,
                };
                match host::http::send(req) {
                    Ok(Some(resp)) if resp.status_code == 200 => {
                        let result: serde_json::Value = serde_json::from_slice(&resp.body)
                            .map_err(|e| format!("bad results response: {e}"))?;
                        crate::net::circuit_clear("acoustid");
                        return _complete_verify_job(cfg, &root, library_id, acoustid_url, existing_id, &result, &unverified, &unverified_key, &job_id_key);
                    }
                    _ => {
                        crate::wasm::log_warn("verify_step: failed to read job results, retrying");
                        crate::net::circuit_mark_failed("acoustid");
                        let _ = crate::store::kv().delete(&job_id_key);
                        crate::wasm::enqueue_verify_task(library_id)?;
                        return Ok((ScanOutcome::More, 0));
                    }
                }
            }
            _ => {
                // Status check failed - job may have expired. Start fresh.
                crate::wasm::log_warn("verify_step: job status check failed, starting new job");
                crate::net::circuit_mark_failed("acoustid");
                let _ = crate::store::kv().delete(&job_id_key);
                // Re-enqueue without loading the 40K unverified list.
                // Next call will find no job and take the send-batches path.
                crate::wasm::enqueue_verify_task(library_id)?;
                return Ok((ScanOutcome::More, 0));
            }
        }
    }

    // No active job — load unverified list and send batches.
    // Use cursor to send only the next 500 entries, avoiding loading the full list.
    let unverified = load_unverified(cfg, library_id, &indexed_key, &unverified_key);

    // Set total for ETA calculation.
    let _ = crate::store::kv().set(&format!("scan.verify_total.{library_id}"), unverified.len().to_string().into_bytes());

    if unverified.is_empty() {
        crate::wasm::log_info("verify_step: all files verified, transitioning to group");
        let _ = crate::store::kv().delete(&unverified_key);
        let _ = crate::store::kv().set(&format!("scan.donev2.{library_id}"), b"1".to_vec());
        crate::wasm::enqueue_group_task(library_id)?;
        post_scan_status(cfg, library_id, 0, "verification complete");
        return Ok((ScanOutcome::Done, 0));
    }

    // Send batches to sidecar. Use cursor to send only the next batch (500 files)
    // instead of loading the full unverified list on every call.
    let batch_size = 100;
    // ponytail: 500 results → 1000 KV ops → WASM 30s deadline exceeded.
    // 50 files × ~2 KV ops = 100 ops, completes in ~5s.
    let max_per_task = 50;
    let verify_cursor_key = format!("verify_cursor.{library_id}");
    let cursor: usize = crate::store::kv()
        .get(&verify_cursor_key)
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let safe_cursor = cursor.min(unverified.len());
    let files_to_send: Vec<(String, i64)> = unverified.iter().skip(safe_cursor).take(max_per_task).cloned().collect();
    let total_batches = (files_to_send.len() + batch_size - 1) / batch_size;
    crate::wasm::log_info(&format!(
        "verify_step: sending {} files to acoustid sidecar ({} batches of {}, unverified={})",
        files_to_send.len(), total_batches, batch_size, unverified.len()
    ));

    let mut all_sent = true;
    for (batch_idx, chunk) in files_to_send.chunks(batch_size).enumerate() {
        let batch_files: Vec<serde_json::Value> = chunk.iter().map(|(rel, mtime)| {
            let abs = root.join(rel);
            serde_json::json!({"path": abs.to_string_lossy(), "mtime": mtime})
        }).collect();

        let body = serde_json::json!({
            "job_id": &job_id,
            "batch_index": batch_idx,
            "batch_total": total_batches,
            "files": batch_files,
            "acoustidApiKey": &cfg.acoustid_api_key,
        });

        let req = host::http::HTTPRequest {
            method: "POST".into(),
            url: format!("{}/job", acoustid_url),
            headers: std::collections::HashMap::new(),
            no_follow_redirects: false,
            body: body.to_string().into_bytes(),
            timeout_ms: 10_000,
        };

        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                crate::net::circuit_clear("acoustid");
                crate::wasm::log_info(&format!(
                    "verify_step: sent batch {}/{}", batch_idx + 1, total_batches
                ));
            }
            _ => {
                crate::net::circuit_mark_failed("acoustid");
                crate::wasm::log_warn(&format!(
                    "verify_step: failed to send batch {}/{}", batch_idx + 1, total_batches
                ));
                all_sent = false;
                break;
            }
        }
    }

    if all_sent {
        // Store cursor to skip files already sent on next call.
        let new_cursor = safe_cursor + files_to_send.len();
        let _ = crate::store::kv().set(&verify_cursor_key, new_cursor.to_string().into_bytes());
        // Store job ID for polling on next task
        let _ = crate::store::kv().set(&job_id_key, job_id.into_bytes());
        crate::wasm::enqueue_verify_task(library_id)?;
        post_scan_status(cfg, library_id, 0, &format!(
            "verifying... {}/{} files", new_cursor, unverified.len()
        ));
        Ok((ScanOutcome::More, 0))
    } else {
        // Some batches failed — retry
        crate::wasm::enqueue_verify_task(library_id)?;
        Ok((ScanOutcome::More, 0))
    }
}


fn _process_verify_results(
    cfg: &Config,
    root: &std::path::Path,
    library_id: i32,
    result: &serde_json::Value,
) -> Result<usize, String> {
    let results = result.get("results").and_then(|r| r.as_object()).cloned().unwrap_or_default();
    let mode_apply = cfg.mode == Mode::Apply;
    let write_budget = std::time::Duration::from_secs(10);
    let write_start = std::time::SystemTime::now();
    let mut processed = 0usize;
    let mut filled = 0usize;
    let mut written = 0usize;
    for (path, entry) in &results {
        let rel = path.trim_start_matches(&root.to_string_lossy().to_string())
            .trim_start_matches('/');
        let key = file_key(library_id, rel);
        if let Ok(Some(v)) = crate::store::kv().get(&key) {
            if let Ok(mut val) = serde_json::from_slice::<Value>(&v) {
                if let Some(tags) = val.get_mut("tags") {
                    if let Some(t) = tags.as_object_mut() {
                        if let Some(matches) = entry.get("matches").and_then(|m| m.as_array()) {
                            if let Some(top) = matches.first() {
                                let album_mbid = top.get("releaseGroups")
                                    .and_then(|rg| rg.as_array())
                                    .and_then(|a| a.first())
                                    .and_then(|g| g.get("id"))
                                    .and_then(|id| id.as_str())
                                    .map(String::from)
                                    .unwrap_or_default();
                                // Sidecar sends `recordingId`; keep `id` as a
                                // fallback for old sidecar builds.
                                let recording_mbid = top.get("recordingId")
                                    .or_else(|| top.get("id"))
                                    .and_then(|id| id.as_str())
                                    .map(String::from)
                                    .unwrap_or_default();
                                t.insert("mbid_album".into(), serde_json::Value::String(album_mbid));
                                t.insert("mbid_recording".into(), serde_json::Value::String(recording_mbid));
                            }
                            // Force-fingerprint reparse: the match's artist
                            // resolves placeholder tracks (missing / Unknown /
                            // Various Artists). Fill KV, then the file tag
                            // (apply mode, budgeted — remaining file writes
                            // are lost only if the 10s budget breaks mid-batch;
                            // KV stays correct either way). ponytail: ≤100
                            // entries per job, each write is a few ms.
                            let current = t.get("artist").and_then(|a| a.as_str()).unwrap_or("");
                            let candidate = crate::tags::artist_fill_candidate(
                                current,
                                cfg.min_confidence,
                                matches.iter().filter_map(|m| {
                                    Some((m.get("score")?.as_f64()?, m.get("artist")?.as_str()?))
                                }),
                            );
                            if let Some(artist) = candidate {
                                t.insert("artist".into(), serde_json::Value::String(artist.clone()));
                                filled += 1;
                                if mode_apply && !past(write_start, write_budget) {
                                    match crate::tags::fill_placeholder_artist(&root.join(rel), &artist) {
                                        Ok(true) => written += 1,
                                        Ok(false) => {}
                                        Err(e) => crate::wasm::log_warn(&format!(
                                            "verify: fill artist {rel}: {e}"
                                        )),
                                    }
                                }
                            }
                        }
                        t.insert("_acoustid_checked".into(), serde_json::Value::Bool(true));
                    }
                }
                let _ = crate::store::kv().set(&key, val.to_string().into_bytes());
            }
        }
        processed += 1;
    }
    let fill_note = if filled > 0 {
        format!(", filled artist on {filled} placeholder track(s) ({written} file tag(s) written)")
    } else {
        String::new()
    };
    crate::wasm::log_info(&format!(
        "verify_step: processed {} results from sidecar{}",
        processed, fill_note
    ));
    Ok(processed)
}


fn _complete_verify_job(
    cfg: &Config,
    root: &std::path::Path,
    library_id: i32,
    acoustid_url: &str,
    job_id: &str,
    result: &serde_json::Value,
    unverified: &[(String, i64)],
    unverified_key: &str,
    job_id_key: &str,
) -> Result<(ScanOutcome, usize), String> {
    let processed = _process_verify_results(cfg, root, library_id, result)?;
    // Cleanup job
    let _ = crate::store::kv().delete(job_id_key);
    let _ = host::http::send(host::http::HTTPRequest {
        method: "DELETE".into(),
        url: format!("{}/job?job_id={}", acoustid_url, job_id),
        headers: std::collections::HashMap::new(),
        no_follow_redirects: false,
        body: vec![],
        timeout_ms: 5_000,
    });

    // Remove processed files from unverified list
    let remaining: Vec<(String, i64)> = unverified.iter().skip(processed).cloned().collect();
    if remaining.is_empty() {
        let _ = crate::store::kv().delete(unverified_key);
    } else {
        // Save as paths only to keep KV value small.
        let remaining_paths: Vec<String> = remaining.iter().map(|(r, _)| r.clone()).collect();
        let _ = crate::store::kv().set(unverified_key, serde_json::to_vec(&remaining_paths).unwrap_or_default());
        // Reset cursor since the list was truncated — next send starts from the beginning.
        let _ = crate::store::kv().delete(&format!("verify_cursor.{}", library_id));
        // If sidecar returned 0 results but files remain, log it.
        if processed == 0 {
            crate::wasm::log_warn(&format!(
                "verify_step: sidecar returned 0 results for {} files, retrying",
                remaining.len()
            ));
        }
    }

    if remaining.is_empty() {
        let _ = crate::store::kv().set(&format!("scan.donev2.{}", library_id), b"1".to_vec());
        crate::wasm::enqueue_group_task(library_id)?;
        post_scan_status(cfg, library_id, processed, "verification complete");
        Ok((ScanOutcome::Done, processed))
    } else {
        crate::wasm::enqueue_verify_task(library_id)?;
        post_scan_status(cfg, library_id, processed, &format!(
            "verifying... {}/{} files verified", processed, unverified.len()
        ));
        Ok((ScanOutcome::More, processed))
    }
}

/// AcoustID circuit breaker: when the sidecar drops mid-run, pause the batch
/// After the scan completes: load the index, group files into albums by their
/// tags, and enqueue plan tasks for each group (batched).
/// AcoustID sidecar; files that still can't be identified are left in place.
/// Detect whether Navidrome's underlying database changed under us (a fresh
/// install, a restored/blank DB, or a different data folder). When it has, the
/// plugin's cached scan index, star tallies and play/skip stats are keyed to
/// the OLD database's media file IDs and are now meaningless - so we clear
/// them and rebuild fresh.
///
/// Fingerprint = sorted library mount points + the Navidrome media-file count
/// (via getScanStatus). A fresh DB typically has a count near 0 and/or a
/// different mount set, so it won't match the stored fingerprint.
pub fn detect_db_change(cfg: &Config) {
    let Some(fp) = db_fingerprint(cfg) else { return };
    let key = "db.fingerprint";
    let prev = crate::store::kv()
        .get(key)
        .ok()
        .flatten()
        .map(|v| String::from_utf8_lossy(&v).into_owned());
    let _ = crate::store::kv().set(key, fp.as_bytes().to_vec());
    // No stored fingerprint yet = first run on this DB; nothing to clear.
    let Some(prev) = prev else { return };
    if prev == fp {
        return;
    }
    crate::wasm::log_info(&format!(
        "DB fingerprint changed (was '{prev}', now '{fp}') - clearing stale scan index, star tallies and play/skip stats"
    ));
    clear_stale_state();
}

/// Build a fingerprint of the current Navidrome library state.
fn db_fingerprint(cfg: &Config) -> Option<String> {
    let mut mounts: Vec<String> = Vec::new();
    for &lib_id in &crate::wasm::target_libraries(cfg) {
        if let Ok(root) = library_real_path(lib_id) {
            mounts.push(root);
        }
    }
    mounts.sort();
    // Media-file count from getScanStatus (0 on a fresh DB).
    let count = get_media_count(cfg);
    Some(format!("{}|{}", mounts.join(","), count))
}

/// Query Navidrome's getScanStatus for the media file count.
fn get_media_count(cfg: &Config) -> i64 {
    let user = crate::wasm::scan_user(cfg);
    if user.is_empty() {
        return 0;
    }
    match host::subsonicapi::call(&format!("getScanStatus?u={user}")) {
        Ok(json) => serde_json::from_str::<Value>(&json)
            .ok()
            .and_then(|v| v.pointer("/subsonic-response/scanStatus/count").and_then(|c| c.as_i64()))
            .unwrap_or(0),
        Err(_) => 0,
    }
}

/// Clear keys that are bound to Navidrome media file IDs from the old DB.
fn clear_stale_state() {
    let mut cleared = 0usize;
    for prefix in ["scan.filev2.", "scan.stackv2.", "star.tally.", "star.pub.", "stat.play.", "stat.skip.", "submit.done.", "write.mbid."] {
        if let Ok(keys) = crate::store::kv().list(prefix) {
            for k in keys {
                if crate::store::kv().delete(&k).is_ok() {
                    cleared += 1;
                }
            }
        }
    }
    // Reset the per-run album budget so a fresh DB isn't throttled by leftover
    // maxAlbumsPerRun state.
    let _ = crate::store::kv().delete("run.albums.remaining");
    crate::wasm::log_info(&format!(
        "DB change: cleared {cleared} stale key(s) (scan index, stars, stats)"
    ));
}

/// Returns `(plan_tasks_enqueued, files_grouped)`.
pub fn group_step(cfg: &Config, library_id: i32) -> Result<(usize, usize), String> {
    let real_root = library_real_path(library_id)?;

    // Post group phase so the dashboard shows "Grouping files..."
    post_phase_status(cfg, library_id, "group");
    // ponytail: 24s of 30 — the dup reports below must never cost us the
    // enqueue_plan_tasks call at the end of this step.
    let task_start = std::time::SystemTime::now();
    let task_budget = std::time::Duration::from_secs(24);

    // Skip if verify is still active — the unverified list means verify hasn't
    // finished. Stale group tasks from previous runs can block the queue otherwise.
    let has_unverified = crate::store::kv()
        .get(&format!("scan.unverified.{library_id}"))
        .ok().flatten().is_some();
    if has_unverified {
        crate::wasm::log_info("group_step: deferred — verify still active");
        return Ok((0, 0));
    }

    // Skip if walk or index is still running — paths_key won't exist yet.
    let has_walk = crate::store::kv()
        .get(&format!("scan.walkv2.{library_id}"))
        .ok().flatten().is_some();
    let has_index_cursor = crate::store::kv()
        .get(&format!("scan.index_cursor.{library_id}"))
        .ok().flatten().is_some();
    if has_walk || has_index_cursor {
        crate::wasm::log_info("group_step: deferred — walk/index still active");
        return Ok((0, 0));
    }

    // AcoustID verification is now handled by verify_step (sidecar batch).
    // The group_step just loads verified files from KV.
    // Uses a cursor to resume across multiple task invocations.

    let indexed_key = format!("scan.indexed.{library_id}");
    let cursor_key = format!("scan.group_cursor.{library_id}");
    let entries_key = format!("scan.group_entries.{library_id}");
    let remaining_key = format!("scan.group_remaining.{library_id}");

    // Load the working list. The remaining list itself IS the cursor: each
    // task consumes the list it loads, and only a mid-list budget hit saves a
    // tail. (The old first-chunk truncation only worked when the 15s budget
    // actually hit — with fast KV reads it fell through to the completion
    // path with 500 files, deleted the 42k tail and grouped a sliver.)
    let has_remaining = crate::store::kv().get(&remaining_key).ok().flatten().is_some();

    let mut file_list: Vec<(String, i64)> = if has_remaining {
        // Resuming — load the tail left by the previous task.
        // ponytail: propagate read errors — an outage-miss must fail the task
        // (retry after reconnect), not look like an empty list ending the pass.
        let remaining: Vec<(String, i64)> = crate::store::kv()
            .get(&remaining_key)
            .map_err(|e| format!("group_step: remaining_key read failed: {e}"))?
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default();
        remaining
    } else {
        Vec::new()
    };
    if file_list.is_empty() {
        let paths_key = format!("scan.group_paths.{library_id}");
        let paths: Vec<String> = crate::store::kv()
            .get(&paths_key)
            .map_err(|e| format!("group_step: paths_key read failed: {e}"))?
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default();
        if paths.is_empty() {
            crate::wasm::log_info("group_step: indexed key empty, skipping (walk not complete yet)");
            return Ok((0, 0));
        }
        crate::wasm::log_info(&format!(
            "group_step: loaded {} files from paths key, processing...",
            paths.len()
        ));
        file_list = paths.into_iter().map(|p| (p, 0i64)).collect();
    } else {
        crate::wasm::log_info(&format!(
            "group_step: resuming from remaining list, {} files",
            file_list.len()
        ));
    }
    // Mark grouping active so is_pipeline_active holds stats/meta off the
    // queue while group tasks chain (rewritten on each budget hit, deleted
    // when the pass really completes).
    let _ = crate::store::kv().set(&cursor_key, b"0".to_vec());
    // ponytail: per-task local offset only — the saved tail carries position.
    let mut cursor: usize = 0;

    crate::wasm::log_info(&format!(
        "group_step: reading tags from cursor {}/{}...",
        cursor, file_list.len()
    ));

    // Read tags from individual KV entries in time-budgeted batches.
    let scan_start = std::time::SystemTime::now();
    // 10s, not 15: the completion path (merge + reports + plan enqueue) needs
    // the rest of the 24s budget before the 30s host deadline.
    let time_budget = std::time::Duration::from_secs(10);
    let mut entries: Vec<(String, TrackTags)> = Vec::new();
    // ponytail: 500 KV reads per chunk → slow in WASM. 50 keeps each chunk under budget.
    let batch_size = 50;
    let mut hit_budget = false;

    for chunk in file_list[cursor..].chunks(batch_size) {
        if past(scan_start, time_budget) {
            hit_budget = true;
            break;
        }
        let keys: Vec<String> = chunk
            .iter()
            .map(|(rel, _)| file_key(library_id, rel))
            .collect();
        if cursor == 0 && entries.is_empty() {
            crate::wasm::log_info(&format!(
                "group_step: get_many debug: {} keys, first_key={}, first_rel={}",
                keys.len(),
                keys.first().unwrap_or(&String::new()),
                chunk.first().map(|(r, _)| r.as_str()).unwrap_or("")
            ));
        }
        match crate::store::kv().get_many(keys) {
            Ok(values) => {
                for (rel, _mtime) in chunk {
                    let key = file_key(library_id, rel);
                    if let Some(v) = values.get(&key) {
                        if let Some(t) = parse_file_tags(v) {
                            entries.push((rel.clone(), t));
                        }
                    }
                }
            }
            Err(e) => {
                // ponytail: fail the task on kv errors — swallowing them made the
                // group phase "complete" with 0 files whenever mysql was down.
                crate::wasm::log_warn(&format!("group_step: get_many failed: {e}"));
                return Err(format!("group_step: get_many: {e}"));
            }
        }
        cursor += chunk.len();
        if cursor == batch_size || cursor % 1000 == 0 {
            crate::wasm::log_info(&format!(
                "group_step: {}/{} tags read in {:?}",
                cursor, file_list.len(), since(scan_start)
            ));
        }
    }

    // If we hit the time budget, save remaining list and re-enqueue.
    if hit_budget {
        crate::wasm::log_info(&format!(
            "group_step: budget reached after {:?} at cursor {}, saving tail",
            since(scan_start),
            cursor
        ));
        // Save only the unprocessed remaining files — much smaller than full list.
        let remaining: Vec<(String, i64)> = file_list[cursor..].to_vec();
        let _ = crate::store::kv().set(&remaining_key, serde_json::to_vec(&remaining).unwrap_or_default());
        let _ = crate::store::kv().set(&cursor_key, cursor.to_string().into_bytes());
        // Save this chunk's entries to a numbered key to avoid O(n²)
        // reload-extend-save on every chunk. Merge at the end. Index from the
        // existing key count — entries.len() collides across tasks.
        let idx = crate::store::kv()
            .list(&format!("scan.group_entries.{library_id}."))
            .map_err(|e| format!("group_step: list group entries: {e}"))?
            .len();
        let chunk_entries_key = format!("scan.group_entries.{library_id}.{idx}");
        let _ = crate::store::kv().set(&chunk_entries_key, serde_json::to_vec(&entries).unwrap_or_default());
        crate::wasm::log_info(&format!(
            "group_step: time budget hit, {} remaining files, {} entries this chunk, re-enqueueing",
            remaining.len(), entries.len()
        ));
        crate::wasm::enqueue_group_task(library_id)?;
        return Ok((0, 0));
    }

    // All files processed — load accumulated entries from previous chunks.
    // Previous chunks saved their entries under scan.group_entries.{id}.{count}.
    let mut all_entries: Vec<(String, TrackTags)> = entries;
    // Scan for any chunk entry keys from previous invocations.
    // ponytail: propagate kv errors here — swallowing them merged partial
    // entries during an outage and planned an incomplete pass.
    let chunk_keys = crate::store::kv()
        .list(&format!("scan.group_entries.{library_id}."))
        .map_err(|e| format!("group_step: list group entries: {e}"))?;
    for k in chunk_keys {
        if let Some(v) = crate::store::kv()
            .get(&k)
            .map_err(|e| format!("group_step: read {k}: {e}"))?
        {
            if let Ok(mut prev) = serde_json::from_slice::<Vec<(String, TrackTags)>>(&v) {
                all_entries.append(&mut prev);
            }
        }
        let _ = crate::store::kv().delete(&k);
    }
    let _ = crate::store::kv().delete(&cursor_key);
    let _ = crate::store::kv().delete(&entries_key);
    let _ = crate::store::kv().delete(&remaining_key);
    let _ = crate::store::kv().delete(&format!("scan.group_entries.{library_id}"));
    // Don't delete indexed_key yet — if WASM kills us during plan enqueue,
    // group_step needs it to resume. Delete after plan tasks are enqueued.

    let total_files = all_entries.len();
    // Identity gate: files below minConfidence are split off. With
    // skipUnverified on they go to the artist's Singles folder via the
    // plan_singles task; with it off they're grouped by tags like the rest.
    let (mut verified, mut failed): (Vec<(String, TrackTags)>, Vec<(String, TrackTags)>) =
        all_entries
            .into_iter()
            .partition(|(_, t)| crate::identity::is_verified(t, cfg.min_confidence, None));
    if !cfg.skip_unverified {
        verified.append(&mut failed);
    }

    crate::wasm::log_info(&format!(
        "group_step: {total_files} files loaded for grouping"
    ));

    if cfg.verify_identity {
        let unverified = total_files - verified.len();
        crate::wasm::log_info(&format!(
            "group_step: verified {}/{total_files} files (unverified: {unverified})",
            verified.len()
        ));
    } else {
        crate::wasm::log_info(&format!(
            "group_step: {total_files} files loaded (verify_identity off, all accepted)"
        ));
    }

    // Report-only, nothing moves; both truncate to a partial report rather
    // than cost us the plan enqueue at the end of this step. Essentia first —
    // HTTP compares need the bigger window, cross-dup below is cheap local
    // stats per file.
    if cfg.essentia_fingerprint && !cfg.essentia_url.trim().is_empty() {
        report_essentia_duplicates(cfg, &real_root, &verified, task_start, task_budget);
        crate::wasm::log_info("group_step: essentia-dup report done");
    }
    if cfg.detect_duplicates {
        report_cross_duplicates(cfg, &real_root, &verified, task_start, task_budget);
        crate::wasm::log_info("group_step: cross-dup report done");
    }

    let meta_only = crate::wasm::meta_only_library(cfg).is_some_and(|id| id == library_id);
    let groups = crate::organizer::group_entries(&verified);
    let groups = if meta_only {
        // Read-only enrichment library: every group gets enriched, the
        // maxAlbumsPerRun move budget does not apply.
        groups
    } else {
        apply_album_budget(cfg, groups)
    };
    // Force-fingerprint folder fixup: when the album artist is a placeholder
    // (missing / Unknown / Various Artists) but every track in the group
    // resolves to one real artist, correct the album artist — file tags first,
    // then KV so every downstream reader (plan, NFO, meta_refresh, next pass)
    // sees it. plan_move then re-files folder + file names. Real album artists
    // and true (mixed) compilations are untouched.
    if cfg.force_fingerprint || cfg.force_refingerprint_unknown_artist {
        let by_rel: std::collections::HashMap<&str, &TrackTags> =
            verified.iter().map(|(r, t)| (r.as_str(), t)).collect();
        let apply_writes = cfg.mode == Mode::Apply;
        let fixup_start = std::time::SystemTime::now();
        // ponytail: ≤8s of the 24s group-task budget — file tag rewrites
        // (lofty) dominate. Checked BETWEEN groups only: file tags are written
        // before the KV patch, so a mid-group kill re-runs the whole group
        // next pass and the placeholder gate makes every write idempotent.
        let fixup_budget = std::time::Duration::from_secs(8);
        let mut collapsed = 0usize;
        let mut truncated = 0usize;
        for (gi, g) in groups.iter().enumerate() {
            if past(fixup_start, fixup_budget) {
                truncated = groups.len() - gi;
                break;
            }
            let Some(cur_aa) = g
                .iter()
                .find_map(|r| by_rel.get(r.as_str()).map(|t| t.album_artist.clone()))
            else {
                continue;
            };
            let tracks: Vec<String> = g
                .iter()
                .filter_map(|r| by_rel.get(r.as_str()).map(|t| t.artist.clone()))
                .collect();
            let Some(new_aa) = crate::organizer::collapse_album_artist(&cur_aa, &tracks) else {
                continue;
            };
            if apply_writes {
                let rr = std::path::Path::new(&real_root);
                for r in g {
                    if let Err(e) =
                        crate::tags::fill_placeholder_album_artist(&rr.join(r), &new_aa)
                    {
                        crate::wasm::log_warn(&format!("group: album_artist {r}: {e}"));
                    }
                }
            }
            for r in g {
                let k = file_key(library_id, r);
                if let Ok(Some(v)) = crate::store::kv().get(&k) {
                    if let Ok(mut val) = serde_json::from_slice::<Value>(&v) {
                        if let Some(obj) = val.get_mut("tags").and_then(|t| t.as_object_mut()) {
                            let same = obj
                                .get("album_artist")
                                .and_then(|a| a.as_str())
                                .map(|a| a == new_aa)
                                .unwrap_or(false);
                            if !same {
                                obj.insert(
                                    "album_artist".into(),
                                    serde_json::Value::String(new_aa.clone()),
                                );
                                let _ = crate::store::kv().set(&k, val.to_string().into_bytes());
                            }
                        }
                    }
                }
            }
            collapsed += 1;
            crate::wasm::log_info(&format!(
                "group_step: force-fingerprint fixup: '{cur_aa}' -> '{new_aa}' ({g:?})"
            ));
        }
        if collapsed > 0 {
            crate::wasm::log_info(&format!(
                "group_step: force-fingerprint fixup re-filed {collapsed} album(s) to their resolved artist"
            ));
        }
        if truncated > 0 {
            crate::wasm::log_info(&format!(
                "group_step: force-fingerprint fixup budget hit — {truncated} group(s) re-checked next pass"
            ));
        }
    }
    crate::wasm::log_info(&format!(
        "group_step: {} album groups{}, enqueueing...",
        groups.len(),
        if meta_only { " (meta-only, unbudgeted)" } else { " after budget" }
    ));
    if cfg.star_tally_enabled {
        let pruned = crate::stats::host_stats::prune_star_tallies();
        if pruned > 0 {
            crate::wasm::log_info(&format!("star: pruned {pruned} orphaned tallie(s)"));
        }
    }
    // meta_refresh reads the indexed list after plan tasks delete
    // scan.indexed — keep an owned copy for it.
    if let Ok(Some(v)) = crate::store::kv().get(&indexed_key) {
        let _ = crate::store::kv().set(&format!("scan.meta_files.{library_id}"), v);
    }
    // Sweep for folders left with no audio: sidecar leftovers merge into the
    // matching album folder (always on); deleting audio-less folders stays
    // behind cleanupNoAudioFolders. Applies to every library the pass
    // processed, meta-only included. Enqueued BEFORE the enrich/plan queue
    // so merges happen ahead of the per-album work, not hours behind it.
    crate::wasm::enqueue_cleanup_task(library_id)?;
    let enqueued = if meta_only {
        // No plan/apply for the destination library. Enrichment is STASHED
        // rather than queued: cleanup (enqueued above) chunks over a 15s
        // budget and its continuation would land behind a ~1-task-per-album
        // burst and starve for hours. cleanup's done branch releases the
        // stash, so the burst can never outrun the sweep that feeds it.
        let _ = crate::store::kv().set(
            &format!("enrich.stash.{library_id}"),
            serde_json::to_vec(&groups).unwrap_or_default(),
        );
        crate::wasm::log_info(&format!(
            "group_step: meta-only library, {} group(s) stashed for enrichment after cleanup",
            groups.len()
        ));
        0
    } else {
        let n = crate::wasm::enqueue_plan_tasks(cfg, library_id, groups)?;
        crate::wasm::log_info(&format!(
            "group_step: grouped {} files into {} plan tasks (enqueued)",
            total_files, n
        ));
        n
    };
    // Safe to delete indexed_key now — plan/enrich tasks are enqueued and group won't need it again.
    let _ = crate::store::kv().delete(&indexed_key);
    // group_paths goes too: it marks "group pending" for enable-time resume,
    // and a late/duplicate group task must no-op instead of re-grouping.
    let _ = crate::store::kv().delete(&format!("scan.group_paths.{library_id}"));
    // Identity gate queue: files below minConfidence for Singles routing.
    // Meta-only libraries never move, so nothing is queued there.
    if !meta_only && !failed.is_empty() {
        let rels: Vec<&String> = failed.iter().map(|(r, _)| r).collect();
        crate::store::kv()
            .set(
                &format!("scan.singles.{library_id}"),
                serde_json::to_vec(&rels).unwrap_or_default(),
            )
            .map_err(|e| format!("group_step: singles queue: {e}"))?;
        crate::wasm::enqueue_singles_task(library_id)?;
        crate::wasm::log_info(&format!(
            "group_step: {} file(s) below minConfidence -> Singles queue",
            failed.len()
        ));
    }
    Ok((enqueued, total_files))
}

/// Route files below the identity threshold (minConfidence) to the artist's
/// Singles folder - the skipUnverified toggle. Drains the queue written by
/// group_step in budgeted chunks; consumed entries never stick even when
/// skipped, so the queue always converges.
pub fn plan_singles_step(cfg: &Config, library_id: i32) -> Result<(usize, usize), String> {
    // The destination library never moves: drop any queue left over from
    // before it became meta-only instead of routing it.
    if crate::wasm::meta_only_library(cfg).is_some_and(|id| id == library_id) {
        let _ = crate::store::kv().delete(&format!("scan.singles.{library_id}"));
        return Ok((0, 0));
    }
    let root = lib_root(library_id)?;
    // moveDestinationLibrary: singles land in the destination library, same
    // contract as plan_move_step's cross-library relocation.
    let dest_root = (!cfg.move_destination_library.trim().is_empty())
        .then(|| crate::wasm::resolve_library_id(&cfg.move_destination_library))
        .flatten()
        .and_then(|id| lib_root(id).ok())
        .unwrap_or_else(|| root.clone());
    let key = format!("scan.singles.{library_id}");
    let Some(raw) = crate::store::kv().get(&key).ok().flatten() else {
        return Ok((0, 0));
    };
    let rels: Vec<String> =
        serde_json::from_slice(&raw).map_err(|e| format!("plan_singles queue: {e}"))?;
    let start = std::time::SystemTime::now();
    // 18s, not 24: tag reads above must leave room for the apply below.
    let budget = std::time::Duration::from_secs(18);
    let mut plan = crate::organizer::GroupPlan {
        bucket: crate::organizer::Bucket::Singles,
        target_dir: cfg.singles_folder.clone(),
        ..Default::default()
    };
    let mut i = 0usize;
    while i < rels.len() {
        if past(start, budget) {
            break;
        }
        let rel = &rels[i];
        i += 1;
        // Gone (already moved by a killed predecessor, or deleted) - consume.
        if !root.join(rel).exists() {
            continue;
        }
        let Some(t) = crate::store::kv()
            .get(&file_key(library_id, rel))
            .ok()
            .flatten()
            .and_then(|v| parse_file_tags(&v))
        else {
            continue;
        };
        // Already sitting in a Singles folder - consume without moving
        // (same-library only; cross-library still owes the move to dest).
        let artist = if t.album_artist.trim().is_empty() {
            cfg.various_folder.clone()
        } else {
            t.album_artist.trim().to_string()
        };
        if dest_root == root
            && (rel.starts_with(&format!("{artist}/{}/", cfg.singles_folder))
                || rel.starts_with(&format!("{}/{}", cfg.various_folder, cfg.singles_folder)))
        {
            continue;
        }
        let to = crate::organizer::singles_target(&dest_root, cfg, &t.album_artist, &t.album, rel);
        if dest_root == root && to.eq_ignore_ascii_case(rel) {
            continue;
        }
        let sidecars = if cfg.rename_sidecars {
            crate::organizer::collect_sidecars(&root, rel)
        } else {
            Vec::new()
        };
        plan.moves.push(crate::organizer::FileMove {
            from: rel.clone(),
            to,
            sidecars,
        });
    }
    let moved = if cfg.mode == crate::config::Mode::Apply {
        if let Err(e) =
            crate::organizer::apply_group_plan_to(&root, &dest_root, &plan, cfg.prune_empty_dirs)
        {
            crate::wasm::log_warn(&format!("plan_singles: {e}"));
        }
        plan.moves.len()
    } else {
        crate::wasm::log_info(&format!(
            "plan_singles: would move {} file(s) to Singles (dry-run)",
            plan.moves.len()
        ));
        0
    };
    let remaining = rels[i..].to_vec();
    if remaining.is_empty() {
        let _ = crate::store::kv().delete(&key);
    } else {
        crate::store::kv()
            .set(&key, serde_json::to_vec(&remaining).unwrap_or_default())
            .map_err(|e| format!("plan_singles queue save: {e}"))?;
        crate::wasm::enqueue_singles_task(library_id)?;
    }
    crate::wasm::log_info(&format!(
        "plan_singles: {moved} file(s) routed to Singles, {} queued",
        remaining.len()
    ));
    Ok((moved, remaining.len()))
}

/// Report files that share an audio fingerprint across the whole library
/// (possible duplicates). Only samples files whose SIZE collides, so exact
/// copies are found without reading every file fully. Report-only.
fn report_cross_duplicates(
    cfg: &Config,
    root: &str,
    verified: &[(String, crate::tags::TrackTags)],
    start: std::time::SystemTime,
    budget: std::time::Duration,
) {
    use std::collections::HashMap;
    let mut truncated = false;
    let mut by_size: HashMap<u64, Vec<String>> = HashMap::new();
    for (rel, _) in verified {
        if remain_ms(start, budget) < 6_000 {
            truncated = true;
            break;
        }
        if let Ok(md) = std::fs::metadata(std::path::Path::new(root).join(rel)) {
            by_size.entry(md.len()).or_default().push(rel.clone());
        }
    }
    let mut fp_map: HashMap<u64, Vec<String>> = HashMap::new();
    'files: for (_, rels) in by_size.iter().filter(|(_, v)| v.len() > 1) {
        for rel in rels {
            if remain_ms(start, budget) < 6_000 {
                truncated = true;
                break 'files;
            }
            if let Some(fp) = content_fingerprint(&std::path::Path::new(root).join(rel)) {
                fp_map.entry(fp).or_default().push(rel.clone());
            }
        }
    }
    if truncated {
        crate::wasm::log_warn(&format!(
            "cross-dup: deadline hit — partial duplicate scan ({} size bucket(s) of {} file(s))",
            by_size.len(),
            verified.len()
        ));
    }
    let dupes: Vec<&Vec<String>> = fp_map.values().filter(|v| v.len() > 1).collect();
    if dupes.is_empty() {
        return;
    }
    let mut summary = String::from("nd-organizer: possible duplicate audio files:\n");
    if truncated {
        summary.push_str("  (partial: scan deadline hit, more pairs next run)\n");
    }
    for d in dupes {
        let joined = d.join(" <=> ");
        crate::wasm::log_warn(&format!("duplicate audio fingerprint: {joined}"));
        summary.push_str(&format!("  {joined}\n"));
    }
    crate::wasm::post_webhook(cfg, &summary);
}

/// Essentia audio-fingerprint duplicate/cover detection. Compares files within
/// each album group using the Essentia sidecar's spectral fingerprint endpoint.
/// Reports covers (50-95% similarity) and duplicates (>95% similarity).
fn report_essentia_duplicates(
    cfg: &Config,
    root: &str,
    verified: &[(String, crate::tags::TrackTags)],
    start: std::time::SystemTime,
    budget: std::time::Duration,
) {
    let base = cfg.essentia_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return;
    }
    let groups = crate::organizer::group_entries(verified);
    let mut covers = Vec::new();
    let mut dupes = Vec::new();
    let mut truncated = false;
    for group in &groups {
        // Compare each pair within the group (cap at 20 files to avoid O(n^2) explosion).
        let limit = group.len().min(20);
        for i in 0..limit {
            for j in (i + 1)..limit {
                // Gate with room for one full capped compare — the 30s task
                // deadline kills everything past it.
                if remain_ms(start, budget) < 9_000 {
                    truncated = true;
                    break;
                }
                let abs_a = std::path::Path::new(root).join(&group[i]);
                let abs_b = std::path::Path::new(root).join(&group[j]);
                let body = serde_json::json!({
                    "path_a": abs_a.to_string_lossy(),
                    "path_b": abs_b.to_string_lossy(),
                });
                let mut headers = std::collections::HashMap::new();
                headers.insert("Content-Type".into(), "application/json".into());
                let req = host::http::HTTPRequest {
                    method: "POST".into(),
                    url: format!("{}/compare", base),
                    headers,
                    no_follow_redirects: false,
                    body: body.to_string().into_bytes(),
                    timeout_ms: cap_ms(start, budget, 8_000),
                };
                if let Ok(Some(resp)) = host::http::send(req) {
                    if resp.status_code == 200 {
                        if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&resp.body) {
                            let sim = val.get("similarity").and_then(|s| s.as_f64()).unwrap_or(0.0);
                            if sim >= 0.95 {
                                dupes.push(format!("{} <=> {} ({:.0}%)", group[i], group[j], sim * 100.0));
                            } else if sim >= 0.5 {
                                covers.push(format!("{} <=> {} ({:.0}%)", group[i], group[j], sim * 100.0));
                            }
                        }
                    }
                }
            }
            if truncated {
                break;
            }
        }
        if truncated {
            break;
        }
    }
    if truncated {
        crate::wasm::log_warn(&format!(
            "essentia-dup: deadline hit — {} pair(s) compared, partial duplicate pass this run",
            dupes.len() + covers.len()
        ));
    }
    if !dupes.is_empty() {
        let summary = format!("nd-organizer: Essentia duplicate audio:\n  {}", dupes.join("\n  "));
        for d in &dupes {
            crate::wasm::log_warn(&format!("essentia duplicate: {d}"));
        }
        crate::wasm::post_webhook(cfg, &summary);
    }
    if !covers.is_empty() {
        let summary = format!("nd-organizer: Essentia possible covers:\n  {}", covers.join("\n  "));
        for c in &covers {
            crate::wasm::log_info(&format!("essentia cover: {c}"));
        }
        crate::wasm::post_webhook(cfg, &summary);
    }
}

/// Cheap content fingerprint: file size + first/last 8 KiB (only read for
/// size-colliding files, so it stays cheap even on large libraries).
fn content_fingerprint(path: &std::path::Path) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let size = f.metadata().ok()?.len();
    let mut head = [0u8; 8192];
    let hlen = f.read(&mut head).ok()?;
    let mut tail = [0u8; 8192];
    let tlen = if size > 16384 {
        f.seek(SeekFrom::End(-8192)).ok()?;
        f.read(&mut tail).ok()?
    } else {
        0
    };
    Some(crate::state::fnv1a64(&format!("{size}|{hlen}|{tlen}")))
}

/// Delete folders under the library root whose entire subtree contains NO audio
/// files (only images/nfo/lyrics/misc remain). Handles both empty folders and
/// folders left behind after moves. Apply mode only - dry-run reports what would
/// be deleted. Never deletes the library root or anything inside an excluded
/// path. Returns how many folders were removed (or would be, in dry-run).
pub fn cleanup_step(cfg: &Config, library_id: i32) -> Result<usize, String> {
    let root = lib_root(library_id)?;
    post_phase_status(cfg, library_id, "cleanup");
    let dry = cfg.mode != crate::config::Mode::Apply;
    let stack_key = format!("cleanup.stack.{library_id}");
    let deleted_key = format!("cleanup.deleted.{library_id}");
    let merged_key = format!("cleanup.merged.{library_id}");

    // Resumable walk cursor (see organizer::cleanup_walk). Starts at the
    // root; persists between chunks so a slow library is fully covered —
    // the old up-front 12s collect silently truncated mid-alphabet and
    // reported the partial walk as a complete pass.
    let mut stack: Vec<(String, bool)> = crate::store::kv()
        .get(&stack_key)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or_else(|| vec![(String::new(), false)]);

    let mut deleted: usize = crate::store::kv()
        .get(&deleted_key)
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut merged: usize = crate::store::kv()
        .get(&merged_key)
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let budget = std::time::Duration::from_secs(15);
    let (lines, m, d, done) = crate::organizer::cleanup_walk(
        &root,
        &mut stack,
        dry,
        cfg.cleanup_no_audio_folders,
        cfg.skip_hidden_files,
        &cfg.exclude_paths,
        budget,
    );
    merged += m;
    deleted += d;
    for l in lines {
        crate::wasm::log_info(&l);
    }

    if done {
        let _ = crate::store::kv().delete(&stack_key);
        let _ = crate::store::kv().delete(&deleted_key);
        let _ = crate::store::kv().delete(&merged_key);
        crate::wasm::log_info(&format!(
            "cleanup: {} sidecar folder(s) merged, {} no-audio folder(s) {}",
            merged,
            deleted,
            if dry { "would be deleted (dry-run)" } else { "deleted" }
        ));
        // Sweep complete: release the per-album stash group_step stashed.
        // Enqueued here (not at group time) so the burst cannot fill the queue
        // ahead of cleanup's chunked continuation.
        // Force-fingerprint reparse: the destination library's moves are part
        // of the correction (folders re-filed under the resolved artist), so
        // release as plan tasks in BOTH modes — dry produces the would-move
        // report, apply moves then enriches. Otherwise the old enrich-only
        // release (apply mode only; the stash survives a dry pass for later).
        let stash_key = format!("enrich.stash.{library_id}");
        if let Ok(Some(raw)) = crate::store::kv().get(&stash_key) {
            if let Ok(groups) = serde_json::from_slice::<Vec<Vec<String>>>(&raw) {
                let force_moves =
                    cfg.force_fingerprint || cfg.force_refingerprint_unknown_artist;
                if force_moves {
                    let n = crate::wasm::enqueue_plan_tasks(cfg, library_id, groups)?;
                    crate::wasm::log_info(&format!(
                        "cleanup: released {n} stashed plan task(s) (force-fingerprint reparse)"
                    ));
                    let _ = crate::store::kv().delete(&stash_key);
                } else if !dry {
                    let n = crate::wasm::enqueue_enrich_tasks(library_id, &groups, 0, 1)?;
                    crate::wasm::log_info(&format!(
                        "cleanup: released {n} stashed enrich task(s)"
                    ));
                    let _ = crate::store::kv().delete(&stash_key);
                }
            }
        }
    } else {
        let _ =
            crate::store::kv().set(&stack_key, serde_json::to_vec(&stack).unwrap_or_default());
        let _ = crate::store::kv().set(&deleted_key, deleted.to_string().into_bytes());
        let _ = crate::store::kv().set(&merged_key, merged.to_string().into_bytes());
        crate::wasm::enqueue_cleanup_task(library_id)?;
        crate::wasm::log_info(&format!(
            "cleanup: {} merged, {} deleted so far, {} dirs remaining",
            merged,
            deleted,
            stack.len()
        ));
    }
    Ok(deleted)
}

/// Cap how many albums a single scheduled pass plans (`maxAlbumsPerRun`).
/// The budget is reset by run_pass; leftover albums are planned on later passes,
/// keeping big reorganizations incremental and playback-friendly. 0 = unlimited.
fn apply_album_budget(cfg: &Config, groups: Vec<Vec<String>>) -> Vec<Vec<String>> {
    if cfg.max_albums_per_run == 0 {
        return groups;
    }
    let key = "run.albums.remaining";
    let remaining: i64 = crate::store::kv()
        .get(key)
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8_lossy(&v).parse().ok())
        .unwrap_or(cfg.max_albums_per_run as i64);
    if remaining <= 0 {
        crate::wasm::log_info(&format!(
            "maxAlbumsPerRun ({}) reached this pass - {} album(s) deferred to a later pass",
            cfg.max_albums_per_run,
            groups.len()
        ));
        return Vec::new();
    }
    let take = (remaining as usize).min(groups.len());
    let _ = crate::store::kv().set(key, (remaining - take as i64).to_string().into_bytes());
    groups.into_iter().take(take).collect()
}

/// When Lidarr knows the artist AND has a single/EP matching the group's first
/// track, re-label a Singles plan from Lidarr's entry (canonical artist +
/// single title) instead of the parent album's tag meta.
fn lidarr_single_relabel(
    cfg: &Config,
    info: &crate::organizer::AlbumInfo,
    files: &[(String, TrackTags)],
    plan: &mut crate::organizer::GroupPlan,
) {
    if cfg.lidarr_url.trim().is_empty() || cfg.lidarr_api_key.trim().is_empty() {
        return;
    }
    let artist = if info.album_artist.trim().is_empty() {
        match info.distinct_artists.first() {
            Some(a) => a.trim().to_string(),
            None => return,
        }
    } else {
        info.album_artist.trim().to_string()
    };
    let title = files
        .first()
        .map(|(_, t)| t.title.trim().to_string())
        .unwrap_or_default();
    if title.is_empty() {
        return;
    }
    let Some(s) = crate::lidarr::host_lidarr::find_single(cfg, &artist, &title) else {
        return;
    };
    let mut patched = info.clone();
    patched.album = s.title;
    patched.album_artist = s.artist;
    let new_dir = crate::organizer::target_album_dir(plan.bucket, &patched, cfg, &title);
    if new_dir != plan.target_dir {
        crate::wasm::log_info(&format!(
            "Lidarr single: relabelling '{}' -> '{}'",
            plan.target_dir, new_dir
        ));
        crate::organizer::relabel_plan(plan, &new_dir);
    }
}

/// Plan and apply file moves for a batch of album groups. Local I/O only
/// (file moves, NFO writes, rollback records). In dry-run mode, generates the
/// full report. In apply mode, posts a lightweight status and returns — network
/// enrichment is handled by `plan_enrich_step`.
pub fn plan_move_step(
    cfg: &Config,
    library_id: i32,
    groups: &[Vec<String>],
    batch_index: i32,
    batch_total: i32,
) -> Result<(), String> {
    post_phase_status(cfg, library_id, "enrich");
    let eff = crate::wasm::effective_config(cfg);
    let cfg = &eff;
    crate::wasm::log_info(&format!(
        "plan_move: batch {}/{}, {} album(s), mode={:?}",
        batch_index + 1, batch_total, groups.len(), cfg.mode
    ));
    let root = lib_root(library_id)?;
    let mut report_parts = Vec::new();
    let mut actions: Vec<serde_json::Value> = Vec::new();
    let mut total_moves = 0usize;
    let mut total_dupes = 0usize;
    let mut total_to_move = 0usize;
    let mut plans: Vec<serde_json::Value> = Vec::new();
    let move_start = std::time::SystemTime::now();
    let move_budget = std::time::Duration::from_secs(15);
    let task_budget = std::time::Duration::from_secs(24);

    for (gi, group) in groups.iter().enumerate() {
        if past(move_start, move_budget) {
            // Re-enqueue remaining groups.
            let remaining = &groups[gi..];
            crate::wasm::log_info(&format!(
                "plan_move: time budget hit after {} albums, re-enqueueing {} remaining",
                gi, remaining.len()
            ));
            if let Err(e) = crate::wasm::enqueue_plan_tasks(cfg, library_id, remaining.to_vec()) {
                crate::wasm::log_warn(&format!("plan_move: re-enqueue failed: {e}"));
            }
            break;
        }
        let mut files: Vec<(String, TrackTags)> = Vec::new();
        for rel in group {
            let key = file_key(library_id, rel);
            if let Ok(Some(v)) = crate::store::kv().get(&key) {
                if let Ok(val) = serde_json::from_slice::<Value>(&v) {
                    if let Some(tags) = val.get("tags") {
                        if !tags.is_null() {
                            if let Ok(t) = serde_json::from_value::<TrackTags>(tags.clone()) {
                                files.push((rel.clone(), t));
                            }
                        }
                    }
                }
            }
        }
        if files.is_empty() {
            continue;
        }
        let folder_hint = group
            .first()
            .and_then(|p| p.rsplit_once('/').map(|(d, _)| d.to_string()))
            .unwrap_or_default();
        let mut info = crate::organizer::album_info_from_tags(&files);
        if info.album.is_empty() {
            // Last segment of the source dir only; the full path would break
            // the MB lookup below (empty/wrong album -> no year, no type).
            info.album = folder_hint.rsplit('/').next().unwrap_or("").to_string();
        }
        let mb_release = if cfg.classify_from_mb
            && cfg.primary_source == crate::config::PrimarySource::MusicBrainz
        {
            if remain_ms(move_start, task_budget) >= 9_000 {
                crate::musicbrainz::lookup(&info.album_artist, &info.album, &cfg.musicbrainz_token)
            } else {
                crate::wasm::log_info("plan_move: MB classify skipped (deadline)");
                None
            }
        } else {
            None
        };
        let mb_type = mb_release
            .as_ref()
            .map(|r| {
                if r.primary_type == "Soundtrack" {
                    "Soundtrack".to_string()
                } else if r.secondary_types.iter().any(|t| {
                    t.eq_ignore_ascii_case("compilation") || t.eq_ignore_ascii_case("live")
                }) || r.primary_type == "Compilation"
                {
                    "Compilation".to_string()
                } else if r.primary_type == "Single" || r.primary_type == "EP" {
                    "Single".to_string()
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();
        if cfg.lidarr_force_search_incomplete
            && !cfg.lidarr_url.trim().is_empty()
            && remain_ms(move_start, task_budget) >= 9_000
        {
            if let Some(album_id) = crate::lidarr::host_lidarr::incomplete_monitored(
                cfg,
                info.track_count,
                &info.album,
                &info.album_artist,
            ) {
                crate::wasm::log_info(&format!(
                    "Lidarr: '{}' - '{}' is incomplete and monitored; submitting AlbumSearch (album {})",
                    info.album_artist, info.album, album_id
                ));
                if remain_ms(move_start, task_budget) >= 9_000 {
                    match crate::lidarr::host_lidarr::force_search(cfg, album_id) {
                        Ok(()) => crate::wasm::log_info("Lidarr AlbumSearch submitted"),
                        Err(e) => crate::wasm::log_warn(&format!("Lidarr AlbumSearch failed: {e}")),
                    }
                }
            }
        }
        let mut plan = crate::organizer::build_group_plan(
            &root,
            cfg,
            &files,
            &folder_hint,
            &mb_type,
            mb_release.as_ref().and_then(|r| r.date.as_deref()),
        );
        if crate::wasm::meta_only_library(cfg).is_some_and(|id| id == library_id) {
            // Destination library: singles routing is disabled there
            // (plan_singles never runs for it) and files only move under the
            // force-fingerprint reparse toggles. plan_enrich applies the same
            // rule so its post-move path mapping stays truthful.
            let force_moves =
                cfg.force_fingerprint || cfg.force_refingerprint_unknown_artist;
            if plan.bucket == crate::organizer::Bucket::Singles || !force_moves {
                plan.moves.clear();
            }
        } else if plan.bucket == crate::organizer::Bucket::Singles
            && remain_ms(move_start, task_budget) >= 9_000
        {
            lidarr_single_relabel(cfg, &info, &files, &mut plan);
        }
        total_moves += plan.moves.len();
        total_dupes += plan.duplicates.len();
        total_to_move += usize::from(!plan.moves.is_empty());
        report_parts.push(group_report(&plan, cfg.mode != Mode::Apply));
        if cfg.star_tally_enabled {
            let mut star_lines: Vec<String> = Vec::new();
            for (rel, _) in &files {
                let abs = root.join(rel).to_string_lossy().to_string();
                if let Some((stars, plays)) = crate::stats::host_stats::star_summary(&abs) {
                    star_lines.push(format!("    - {rel}: {stars} stars ({plays} playcount)"));
                }
            }
            if !star_lines.is_empty() {
                report_parts.push(format!(
                    "  Star ratings (tally preview):\n{}",
                    star_lines.join("\n")
                ));
            }
        }
        plans.push(serde_json::json!({
            "kind": match plan.bucket {
                crate::organizer::Bucket::Soundtrack => "soundtrack",
                crate::organizer::Bucket::Various => "various",
                crate::organizer::Bucket::Singles => "singles",
                crate::organizer::Bucket::Normal => "normal",
            },
            "album": info.album,
            "albumArtist": info.album_artist,
            "year": info.year,
            "trackCount": info.track_count,
            "target": plan.target_dir,
            "moves": plan.moves.iter().map(|m| serde_json::json!({"from": m.from, "to": m.to})).collect::<Vec<_>>(),
            "duplicates": plan.duplicates.len(),
            "fillers": plan.fillers.len(),
        }));

        if cfg.mode == Mode::Apply && !plan.moves.is_empty() {
            crate::organizer::apply_group_plan(&root, &plan, cfg.prune_empty_dirs)?;
            if !cfg.move_destination_library.is_empty() {
                let dest_id = crate::wasm::resolve_library_id(&cfg.move_destination_library);
                if let Some(dest_id) = dest_id {
                    let source_album_dir = root.join(&plan.target_dir);
                    if let Ok(dest_root) = lib_root(dest_id) {
                        let dest_album_dir = dest_root.join(&plan.target_dir);
                        if source_album_dir != dest_album_dir {
                            match crate::organizer::move_album_folder(&source_album_dir, &dest_album_dir) {
                                Ok(n) => {
                                    crate::wasm::log_info(&format!(
                                        "cross-library move: {} file(s) -> {}",
                                        n, cfg.move_destination_library
                                    ));
                                    actions.push(serde_json::json!({
                                        "ts": crate::state::now_ts(),
                                        "text": format!("cross-library move: {n} file(s) to {}", cfg.move_destination_library),
                                    }));
                                }
                                Err(e) => {
                                    crate::wasm::log_warn(&format!("cross-library move failed: {e}"));
                                }
                            }
                        }
                    } else {
                        crate::wasm::log_warn(&format!(
                            "cross-library move: destination \"{}\" not accessible",
                            cfg.move_destination_library
                        ));
                    }
                } else {
                    crate::wasm::log_warn(&format!(
                        "cross-library move: destination \"{}\" not found",
                        cfg.move_destination_library
                    ));
                }
            }
            for m in &plan.moves {
                actions.push(serde_json::json!({
                    "ts": crate::state::now_ts(),
                    "text": format!("moved {} -> {}", m.from, m.to),
                }));
            }
            if cfg.scan_after_album && remain_ms(move_start, task_budget) >= 6_000 {
                if let Err(e) = crate::wasm::trigger_navidrome_scan(cfg) {
                    crate::wasm::log_warn(&format!("early scan trigger failed: {e}"));
                }
            }
            let run_id = crate::wasm::current_run_id(library_id)?;
            let mut nfo_backup_key: Option<String> = None;
            let nfo_abs = root.join(&plan.target_dir).join("album.nfo");
            if let Ok(orig) = std::fs::read(&nfo_abs) {
                if let Ok(seq) = crate::state::host_state::next_seq(&run_id) {
                    let key = crate::state::backup_key(&run_id, seq);
                    if crate::store::kv().set(&key, orig).is_ok() {
                        nfo_backup_key = Some(key);
                    }
                }
            }
            for (i, m) in plan.moves.iter().enumerate() {
                crate::stats::host_stats::migrate_star_tally(
                    &root.join(&m.from).to_string_lossy(),
                    &root.join(&m.to).to_string_lossy(),
                );
                let from_dir = dirname(&m.from).to_string();
                let to_dir = dirname(&m.to).to_string();
                let mut rec = crate::state::ApplyRecord {
                    seq: 0,
                    ts: crate::state::now_ts(),
                    run_id: run_id.clone(),
                    library_id,
                    from_dir,
                    to_dir,
                    file_renames: vec![crate::state::FileRename {
                        from: basename(&m.from).to_string(),
                        to: basename(&m.to).to_string(),
                    }],
                    dir_sidecars: vec![],
                    nfo_written: if i == 0 {
                        Some(format!("{}/album.nfo", plan.target_dir))
                    } else {
                        None
                    },
                    nfo_backup: if i == 0 { nfo_backup_key.clone() } else { None },
                };
                if let Err(e) = crate::state::host_state::record_apply(&mut rec) {
                    crate::wasm::log_warn(&format!("record apply {}: {e}", m.from));
                }
            }
        } else if cfg.mode != Mode::Apply {
            for m in &plan.moves {
                actions.push(serde_json::json!({
                    "ts": crate::state::now_ts(),
                    "text": format!("would move {} -> {}", m.from, m.to),
                }));
            }
        }
    }

    // Dry-run: generate and post the full report now (no enrichment needed).
    if cfg.mode != Mode::Apply {
        let mut report_text = if report_parts.is_empty() {
            format!(
                "No albums in batch {}/{}\n",
                batch_index + 1,
                batch_total.max(1)
            )
        } else {
            report_parts.join("\n")
        };
        report_text = format!(
            "[DRY RUN] batch {}/{} - simulated, nothing changed.\n\
             Switch mode to 'apply' to execute exactly these actions.\n{}\n",
            batch_index + 1,
            batch_total.max(1),
            report_text
        );
        let run_id = crate::wasm::current_run_id(library_id).unwrap_or_default();
        report_text.push_str(&format!(
            "\n[rollback] Run ID: {run_id}\nTo undo everything in this run, set 'rollbackRunId' = {run_id} in the plugin settings, then run a pass.\n"
        ));
        crate::wasm::save_report(&report_text, cfg.backup_retention_days as i64);
        crate::wasm::log_info(&report_text);
        let report_envelope = serde_json::json!({
            "ts": crate::state::now_ts(),
            "mode": crate::wasm::mode_label(cfg),
            "kind": "report",
            "dryRun": true,
            "batch": { "index": batch_index, "total": batch_total },
            "runId": run_id,
            "text": report_text,
            "plans": plans,
            "actions": actions,
            "libraries": [{
                "id": library_id,
                "albumsFound": groups.len(),
                "albumsToMove": total_to_move,
                "fileMoves": total_moves,
                "duplicates": total_dupes,
                "kept": 0,
                "skipped": 0
            }],
        })
        .to_string();
        crate::wasm::post_webhook(cfg, &report_envelope);
        return Ok(());
    }

    // Apply mode: post lightweight status. Enrichment is handled by
    // plan_enrich_step which gets enqueued per-album below.
    let status_json = serde_json::json!({
        "ts": crate::state::now_ts(),
        "mode": "apply",
        "inProgress": true,
        "phase": "plan",
        "batch": { "index": batch_index, "total": batch_total },
        "libraries": [{
            "id": library_id,
            "albumsFound": groups.len(),
            "albumsToMove": total_to_move,
            "fileMoves": total_moves,
            "duplicates": total_dupes,
            "kept": 0,
            "skipped": 0
        }],
        "totalAlbumsToMove": total_to_move,
        "totalFileMoves": total_moves,
        "plans": plans,
        "actions": actions,
        "warnings": [],
        "integrations": crate::wasm::integration_health(cfg),
        "tasks": crate::wasm::task_log(),
    })
    .to_string();
    crate::wasm::post_webhook(cfg, &status_json);
    Ok(())
}

/// Network-heavy enrichment for a batch of album groups: auto-tag, ReplayGain,
/// artwork, lyrics, genre, acoustic tags, essentia, Lidarr refresh, AudioMuse
/// re-sync. Runs as a separate WASM task per album to stay under the 30s
/// deadline.
pub fn plan_enrich_step(
    cfg: &Config,
    library_id: i32,
    groups: &[Vec<String>],
    batch_index: i32,
    batch_total: i32,
) -> Result<(), String> {
    post_phase_status(cfg, library_id, "enrich");
    let eff = crate::wasm::effective_config(cfg);
    let cfg = &eff;
    let root = lib_root(library_id)?;
    let meta_only = crate::wasm::meta_only_library(cfg).is_some_and(|id| id == library_id);

    // Log enrichment plan for this album.
    let mut enrichments: Vec<&str> = Vec::new();
    if cfg.auto_tag_from_mb { enrichments.push("auto-tag"); }
    if cfg.write_replaygain { enrichments.push("ReplayGain"); }
    if cfg.embed_artwork || cfg.write_cover_jpg { enrichments.push("artwork"); }
    if !cfg.lyrics_source.is_empty() { enrichments.push("lyrics"); }
    if !cfg.genre_source.is_empty() { enrichments.push("genre"); }
    if cfg.write_acoustic_tags && !cfg.audiomuse_url.trim().is_empty() { enrichments.push("acoustic-tags"); }
    if cfg.genre_source == "essentia" && !cfg.essentia_url.trim().is_empty() { enrichments.push("essentia-genres"); }
    if cfg.lidarr_mode == crate::config::LidarrMode::MetadataPlusRescan && !cfg.lidarr_url.trim().is_empty() { enrichments.push("lidarr-refresh"); }
    if cfg.scan_after_tag_write { enrichments.push("navidrome-rescan"); }
    if cfg.write_nfo { enrichments.push("nfo"); }
    if cfg.verify_instrumental && !cfg.essentia_url.trim().is_empty() { enrichments.push("instrumental-check"); }
    if cfg.verify_acoustic && !cfg.essentia_url.trim().is_empty() { enrichments.push("acoustic-check"); }
    crate::wasm::log_info(&format!(
        "enrich_step: batch {}/{}, {} album(s), plan: [{}]",
        batch_index + 1, batch_total, groups.len(), enrichments.join(", ")
    ));
    let mut report_parts = Vec::new();
    let mut actions: Vec<serde_json::Value> = Vec::new();
    let mut total_autotags = 0usize;
    let mut total_replaygains = 0usize;
    let enrich_start = std::time::SystemTime::now();
    let enrich_budget = std::time::Duration::from_secs(15);
    let task_budget = std::time::Duration::from_secs(24);

    for group in groups {
        // Check time budget before starting each album.
        // Cached sidecar results make re-processing fast on resume.
        if past(enrich_start, enrich_budget) {
            crate::wasm::log_info("enrich_step: time budget hit, pausing (cached results on resume)");
            break;
        }
        let mut files: Vec<(String, TrackTags)> = Vec::new();
        for rel in group {
            let key = file_key(library_id, rel);
            if let Ok(Some(v)) = crate::store::kv().get(&key) {
                if let Ok(val) = serde_json::from_slice::<Value>(&v) {
                    if let Some(tags) = val.get("tags") {
                        if !tags.is_null() {
                            if let Ok(t) = serde_json::from_value::<TrackTags>(tags.clone()) {
                                files.push((rel.clone(), t));
                            }
                        }
                    }
                }
            }
        }
        if files.is_empty() {
            continue;
        }
        let folder_hint = group
            .first()
            .and_then(|p| p.rsplit_once('/').map(|(d, _)| d.to_string()))
            .unwrap_or_default();
        let mut info = crate::organizer::album_info_from_tags(&files);
        if info.album.is_empty() {
            // Last segment of the source dir only; the full path would break
            // the MB lookup below (empty/wrong album -> no year, no type).
            info.album = folder_hint.rsplit('/').next().unwrap_or("").to_string();
        }
        let mb_release = if cfg.classify_from_mb
            && cfg.primary_source == crate::config::PrimarySource::MusicBrainz
        {
            if remain_ms(enrich_start, task_budget) >= 9_000 {
                crate::musicbrainz::lookup(&info.album_artist, &info.album, &cfg.musicbrainz_token)
            } else {
                None
            }
        } else {
            None
        };
        let mut plan = crate::organizer::build_group_plan(&root, cfg, &files, &folder_hint, &mb_release.as_ref().map(|r| {
            if r.primary_type == "Soundtrack" { "Soundtrack".to_string() }
            else if r.secondary_types.iter().any(|t| t.eq_ignore_ascii_case("compilation") || t.eq_ignore_ascii_case("live")) || r.primary_type == "Compilation" { "Compilation".to_string() }
            else if r.primary_type == "Single" || r.primary_type == "EP" { "Single".to_string() }
            else { String::new() }
        }).unwrap_or_default(), mb_release.as_ref().and_then(|r| r.date.as_deref()));
        if meta_only {
            // Force-fingerprint reparse + apply: plan_move in this task's
            // enqueue chain already executed the moves (dry runs never reach
            // enrich; a stale enrich enqueued before the toggles went on runs
            // without mode==apply and falls through to the safe branch).
            let keep_moves = (cfg.force_fingerprint || cfg.force_refingerprint_unknown_artist)
                && cfg.mode == Mode::Apply;
            if plan.bucket == crate::organizer::Bucket::Singles || !keep_moves {
                // Read-only for this plan: files stay put (singles routing is
                // disabled in the destination library; moves only run under
                // force-fingerprint reparse). Clear planned moves and point
                // the target at the group's actual directory so cover.jpg/album.nfo
                // land in place instead of a would-be folder.
                plan.moves.clear();
                if let Some(dir) = files
                    .first()
                    .and_then(|(r, _)| root.join(r).parent().map(|p| p.to_path_buf()))
                {
                    plan.target_dir = dir
                        .strip_prefix(&root)
                        .unwrap_or(&dir)
                        .to_string_lossy()
                        .replace('\\', "/");
                }
            }
        } else if plan.bucket == crate::organizer::Bucket::Singles
            && remain_ms(enrich_start, task_budget) >= 9_000
        {
            lidarr_single_relabel(cfg, &info, &files, &mut plan);
        }
        report_parts.push(group_report(&plan, false));

        // plan_move_step applied the moves before this task ran (Apply mode):
        // resolve every rel to its post-move location, including the
        // cross-library relocation to moveDestinationLibrary.
        let dest_root = (!cfg.move_destination_library.trim().is_empty())
            .then(|| crate::wasm::resolve_library_id(&cfg.move_destination_library))
            .flatten()
            .and_then(|id| lib_root(id).ok());
        let cross =
            !plan.moves.is_empty() && dest_root.as_deref().is_some_and(|d| d != root.as_path());
        let dst_of = |rel: &str| -> std::path::PathBuf {
            match &dest_root {
                Some(d) if cross => d.join(rel),
                _ => root.join(rel),
            }
        };
        let fin: std::collections::HashMap<String, std::path::PathBuf> = files
            .iter()
            .map(|(r, _)| (r.clone(), root.join(r)))
            .chain(plan.moves.iter().map(|m| (m.from.clone(), dst_of(&m.to))))
            .collect();
        let target_abs = dst_of(&plan.target_dir);
        let abs_of =
            |rel: &str| -> std::path::PathBuf { fin.get(rel).cloned().unwrap_or_else(|| root.join(rel)) };
        // Sidecar output stays with the audio: when plan_move hasn't brought
        // this group's files into the target yet, writing there would stock a
        // sidecar-only twin folder. Fall back to where the files actually are;
        // the move carries the sidecars over together with them.
        let sidecar_abs = if meta_only || crate::organizer::dir_has_audio(&target_abs) {
            target_abs.clone()
        } else {
            files
                .iter()
                .map(|(r, _)| abs_of(r))
                .find(|p| p.exists())
                .or_else(|| files.first().map(|(r, _)| root.join(r)))
                .and_then(|p| p.parent().map(|x| x.to_path_buf()))
                .unwrap_or_else(|| target_abs.clone())
        };

        if !files.is_empty() {
            if cfg.auto_tag_from_mb && remain_ms(enrich_start, task_budget) >= 9_000 {
                if let Some(rel) = &mb_release {
                    if let Some(tagged) = autotag_album(cfg, &files, rel, &info, &fin) {
                        total_autotags += tagged;
                        actions.push(serde_json::json!({
                            "ts": crate::state::now_ts(),
                            "text": format!(
                                "auto-tagged {tagged} track(s) from MusicBrainz ({})",
                                info.album
                            ),
                        }));
                    }
                }
            }
            if cfg.write_replaygain {
                let mut rg_entries: Vec<(std::path::PathBuf, f64, Option<f64>)> = Vec::new();
                for (rel, _) in &files {
                    if past(enrich_start, enrich_budget) { break; }
                    let abs = abs_of(rel);
                    if let Some((gain, peak)) = replaygain_for(cfg, &abs.to_string_lossy()) {
                        if crate::tags::write_replaygain(&abs, gain, peak, cfg.overwrite_existing_tags)
                            .unwrap_or(false)
                        {
                            rg_entries.push((abs, gain, peak));
                        }
                    }
                }
                if !rg_entries.is_empty() {
                    total_replaygains += rg_entries.len();
                    if cfg.replay_gain_mode == "album" {
                        let n = rg_entries.len() as f64;
                        let album_gain = rg_entries.iter().map(|(_, g, _)| g).sum::<f64>() / n;
                        let album_peak = rg_entries
                            .iter()
                            .filter_map(|(_, _, p)| *p)
                            .fold(f64::NEG_INFINITY, f64::max);
                        for (abs, _, _) in &rg_entries {
                            if past(enrich_start, enrich_budget) { break; }
                            let peak = if album_peak.is_finite() { Some(album_peak) } else { None };
                            if crate::tags::write_replaygain_album(
                                abs,
                                album_gain,
                                peak,
                                cfg.overwrite_existing_tags,
                            )
                            .is_err()
                            {
                                crate::wasm::log_warn(&format!("write album replaygain {}", abs.display()));
                            }
                        }
                    }
                    actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": format!(
                            "wrote ReplayGain tags for {} track(s) ({}){}",
                            rg_entries.len(),
                            info.album,
                            if cfg.replay_gain_mode == "album" { " + album tags" } else { "" }
                        ),
                    }));
                }
            }
            if (cfg.embed_artwork || cfg.write_cover_jpg)
                && cfg.artwork_front
                && remain_ms(enrich_start, task_budget) >= 16_000
            {
                let mbid = files.iter().find_map(|(_, t)| {
                    if !t.mbid_album.trim().is_empty() {
                        Some(t.mbid_album.clone())
                    } else {
                        None
                    }
                });
                if let Some((bytes, source)) = crate::artwork::fetch_with_fallback(
                    cfg,
                    mbid.as_deref(),
                    &info.album_artist,
                    &info.album,
                ) {
                    let dir = sidecar_abs.clone();
                    let mut embedded = 0usize;
                    let mut sidecar = false;
                    if cfg.embed_artwork {
                        let first = files.first().map(|(r, _)| abs_of(r)).unwrap_or_default();
                        if cfg.overwrite_art || !crate::artwork::has_embedded(&first) {
                            for (rel, _) in files.iter() {
                                if past(enrich_start, enrich_budget) { break; }
                                let path = abs_of(rel);
                                if crate::artwork::embed(&path, bytes.clone(), crate::artwork::ArtKind::Front).is_ok() {
                                    embedded += 1;
                                }
                            }
                        }
                    }
                    if cfg.write_cover_jpg {
                        if cfg.overwrite_art || !dir.join("cover.jpg").exists() {
                            if crate::artwork::write_sidecar(&dir, bytes.clone()).is_ok() {
                                sidecar = true;
                            }
                        }
                    }
                    if embedded > 0 || sidecar {
                        actions.push(serde_json::json!({
                            "ts": crate::state::now_ts(),
                            "text": format!("artwork: {source} → {embedded} image(s){}",
                                if sidecar { " + cover.jpg" } else { "" }),
                        }));
                    }
                }
            }
            // Extra artwork kinds (Cover Art Archive only): back cover, CD
            // medium art, booklet - embedded when their toggles are on.
            if cfg.embed_artwork
                && (cfg.artwork_back || cfg.artwork_cd || cfg.artwork_booklet)
                && remain_ms(enrich_start, task_budget) >= 9_000
            {
                let mbid = files.iter().find_map(|(_, t)| {
                    if !t.mbid_album.trim().is_empty() {
                        Some(t.mbid_album.clone())
                    } else {
                        None
                    }
                });
                if let Some(mbid) = mbid {
                    let first = files.first().map(|(r, _)| abs_of(r)).unwrap_or_default();
                    // Same overwrite policy as front: one embedded picture
                    // (any kind) counts as "already has art".
                    if cfg.overwrite_art || !crate::artwork::has_embedded(&first) {
                        let mut kinds: Vec<&str> = Vec::new();
                        for (on, kind, label) in [
                            (cfg.artwork_back, crate::artwork::ArtKind::Back, "back"),
                            (cfg.artwork_cd, crate::artwork::ArtKind::Cd, "cd"),
                            (cfg.artwork_booklet, crate::artwork::ArtKind::Booklet, "booklet"),
                        ] {
                            if past(enrich_start, task_budget) {
                                break;
                            }
                            if !on {
                                continue;
                            }
                            if let Some(bytes) = crate::artwork::fetch(&mbid, kind) {
                                let mut n = 0usize;
                                for (rel, _) in files.iter() {
                                    if past(enrich_start, task_budget) {
                                        break;
                                    }
                                    if crate::artwork::embed(&abs_of(rel), bytes.clone(), kind)
                                        .is_ok()
                                    {
                                        n += 1;
                                    }
                                }
                                if n > 0 {
                                    kinds.push(label);
                                }
                            }
                        }
                        if !kinds.is_empty() {
                            actions.push(serde_json::json!({
                                "ts": crate::state::now_ts(),
                                "text": format!("artwork: embedded {} ({})", kinds.join(", "), "coverartarchive"),
                            }));
                        }
                    }
                }
            }
            // AcoustID submit: one POST per file, once ever (submit.done.*
            // dedup key in KV). Own circuit so a dead acoustid.org backs off
            // without touching verify. 12s headroom + 9s HTTP timeout keeps
            // the task under the 30s host kill.
            if cfg.acoustid_submit
                && !cfg.acoustid_api_key.trim().is_empty()
                && !cfg.acoustid_url.trim().is_empty()
                && remain_ms(enrich_start, task_budget) >= 12_000
                && !crate::net::circuit_open("acoustid-submit")
            {
                let url = format!("{}/submit", cfg.acoustid_url.trim().trim_end_matches('/'));
                let mut submitted = 0usize;
                let mut dead = false;
                for (rel, t) in &files {
                    if dead || past(enrich_start, task_budget) {
                        break;
                    }
                    if remain_ms(enrich_start, task_budget) < 12_000 {
                        break;
                    }
                    if t.mbid_recording.trim().is_empty() {
                        continue;
                    }
                    let dk = format!("submit.done.{}", abs_of(rel).display());
                    if crate::store::kv().get(&dk).ok().flatten().is_some() {
                        continue;
                    }
                    if !crate::net::throttle("acoustid-submit", 1000) {
                        break;
                    }
                    let body = serde_json::to_vec(&serde_json::json!({
                        "path": abs_of(rel).to_string_lossy().to_string(),
                        "acoustidApiKey": cfg.acoustid_api_key,
                        "recordingId": t.mbid_recording,
                    }))
                    .unwrap_or_default();
                    let req = host::http::HTTPRequest {
                        method: "POST".into(),
                        url: url.clone(),
                        headers: std::collections::HashMap::from([(
                            "Content-Type".into(),
                            "application/json".into(),
                        )]),
                        no_follow_redirects: false,
                        body,
                        timeout_ms: 9_000,
                    };
                    let ok = match host::http::send(req) {
                        Ok(Some(resp)) if resp.status_code == 200 => {
                            serde_json::from_slice::<serde_json::Value>(&resp.body)
                                .ok()
                                .and_then(|v| v.get("ok").and_then(|b| b.as_bool()))
                                .unwrap_or(false)
                        }
                        _ => false,
                    };
                    if ok {
                        let _ = crate::store::kv().set(&dk, b"1".to_vec());
                        crate::net::circuit_clear("acoustid-submit");
                        submitted += 1;
                    } else {
                        crate::net::circuit_mark_failed("acoustid-submit");
                        dead = true;
                    }
                }
                if submitted > 0 {
                    actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": format!("acoustid: submitted {submitted} fingerprint(s)"),
                    }));
                }
            }
            // Write verified MBIDs from the scan index into the audio files
            // (verify only stores them in KV), so later runs read identities
            // straight from tags. Once-per-path dedup; cleared on DB change.
            if remain_ms(enrich_start, task_budget) >= 9_000 {
                for (rel, t) in &files {
                    if past(enrich_start, task_budget)
                        || remain_ms(enrich_start, task_budget) < 9_000
                    {
                        break;
                    }
                    if t.mbid_album.trim().is_empty() {
                        continue;
                    }
                    let abs = abs_of(rel);
                    let dk = format!(
                        "write.mbid.{:016x}",
                        crate::state::fnv1a64(&abs.to_string_lossy())
                    );
                    if crate::store::kv().get(&dk).ok().flatten().is_some() {
                        continue;
                    }
                    match crate::tags::write_mbids(
                        &abs,
                        &t.mbid_album,
                        Some(t.mbid_recording.trim()),
                        cfg.overwrite_existing_tags,
                    ) {
                        Ok(()) => {
                            let _ = crate::store::kv().set(&dk, b"1".to_vec());
                        }
                        Err(e) => crate::wasm::log_warn(&format!("write mbids {}: {e}", rel)),
                    }
                }
            }
            if (cfg.lyrics_source == "lrclib" || cfg.lyrics_source == "genius")
                && remain_ms(enrich_start, task_budget) >= 11_000
            {
                let n = download_lyrics_for(
                    &plan,
                    &files,
                    &fin,
                    cfg.lyrics_format.as_str(),
                    &cfg.lyrics_source,
                    cfg,
                    enrich_start,
                    task_budget,
                );
                if n > 0 {
                    actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": format!("lyrics: fetched {n} sidecar(s)"),
                    }));
                }
            }
            if !cfg.genre_source.is_empty() && remain_ms(enrich_start, task_budget) >= 2_000 {
                let mbid = files.iter().find_map(|(_, t)| {
                    if !t.mbid_album.trim().is_empty() {
                        Some(t.mbid_album.clone())
                    } else {
                        None
                    }
                });
                let nfo_genres = if cfg.read_nfo {
                    crate::nfo::read_album_nfo(&sidecar_abs)
                        .map(|n| n.genres)
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                if let Some((genres, source)) = fetch_genre_with_fallback(
                    cfg,
                    mbid.as_deref(),
                    &info.album_artist,
                    &info.album,
                    &nfo_genres,
                    enrich_start,
                    task_budget,
                ) {
                    for (rel, _tags) in files.iter() {
                        if past(enrich_start, enrich_budget) { break; }
                        let path = abs_of(rel);
                        let _ = crate::tags::write_genre(&path, &genres);
                    }
                    actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": format!("genre: {source} → {} tag(s)", genres.len()),
                    }));
                }
            }
            if cfg.write_acoustic_tags
                && !cfg.audiomuse_url.trim().is_empty()
                && remain_ms(enrich_start, task_budget) >= 9_000
            {
                let n = write_acoustic_tags_for(cfg, &plan, &files, &fin, enrich_start, task_budget);
                if n > 0 {
                    actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": format!("acoustic tags: BPM/key/mood for {n} track(s)"),
                    }));
                }
            }
            if cfg.genre_source == "essentia" && !cfg.essentia_url.trim().is_empty() {
                let n = crate::stats::host_stats::write_essentia_genres(cfg, &files, &fin, 20_000);
                if n > 0 {
                    actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": format!("essentia genres: {n} track(s) tagged"),
                    }));
                }
            }
            // Pass 1: Verify instrumental performance — trust but verify.
            // If labeled "(Instrumental)", verify and strip if not instrumental.
            // If not labeled but IS instrumental, append "(Instrumental)".
            if cfg.verify_instrumental && !cfg.essentia_url.trim().is_empty() {
                for (rel, tags) in &files {
                    if past(enrich_start, enrich_budget) { break; }
                    let title = tags.title.clone();
                    let abs = abs_of(rel);
                    let path_str = abs.to_string_lossy().to_string();
                    let cache_key = format!("instrumental:{}", path_str);

                    // Check cache first. A failed or timed-out call is
                    // "unknown" (None) — skip the label check rather than
                    // strip a correct "(Instrumental)" on a network error.
                    let is_instrumental: Option<bool> = if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
                        serde_json::from_slice::<serde_json::Value>(&v)
                            .ok()
                            .and_then(|v| v.get("isInstrumental").and_then(|v| v.as_bool()))
                    } else {
                        // Call Essentia /instrumental-check.
                        let base = cfg.essentia_url.trim().trim_end_matches('/');
                        let body = serde_json::json!({"path": path_str});
                        let req = nd_pdk::host::http::HTTPRequest {
                            method: "POST".into(),
                            url: format!("{}/instrumental-check", base),
                            headers: std::collections::HashMap::new(),
                            no_follow_redirects: false,
                            body: body.to_string().into_bytes(),
                            timeout_ms: cap_ms(enrich_start, task_budget, 8_000),
                        };
                        match nd_pdk::host::http::send(req) {
                            Ok(Some(resp)) if resp.status_code == 200 => {
                                let val: serde_json::Value = serde_json::from_slice(&resp.body).ok().unwrap_or_default();
                                let _ = crate::store::kv().set(&cache_key, resp.body);
                                val.get("isInstrumental").and_then(|v| v.as_bool())
                            }
                            _ => None,
                        }
                    };

                    // Re-read title from file tag to avoid stale KV cache
                    // causing double-suffix on subsequent runs.
                    let current_title = crate::tags::read_tags(&abs)
                        .map(|t| t.title)
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| title.clone());
                    let current_lower = current_title.to_lowercase();
                    let current_has_label = current_lower.contains("instrumental");

                    if let Some(is_instrumental) = is_instrumental {
                    if current_has_label && !is_instrumental {
                        // Labeled instrumental but NOT actually instrumental — strip the label.
                        let stripped = crate::tags::strip_instrumental(&current_title);
                        if stripped != current_title {
                            let _ = crate::tags::write_title(&abs, &stripped);
                            crate::wasm::log_info(&format!(
                                "instrumental: stripped from '{}' -> '{}' (not truly instrumental)",
                                current_title, stripped
                            ));
                            actions.push(serde_json::json!({
                                "ts": crate::state::now_ts(),
                                "text": format!("instrumental: stripped from '{}' (not truly instrumental)", current_title),
                            }));
                            if cfg.scan_after_tag_write {
                                let _ = crate::wasm::trigger_navidrome_scan(cfg);
                            }
                        }
                    } else if is_instrumental && !current_has_label {
                        // Instrumental but not labeled — append "(Instrumental)".
                        let new_title = format!("{} (Instrumental)", current_title);
                        let _ = crate::tags::write_title(&abs, &new_title);
                        crate::wasm::log_info(&format!(
                            "instrumental: appended to '{}' -> '{}'",
                            current_title, new_title
                        ));
                        actions.push(serde_json::json!({
                            "ts": crate::state::now_ts(),
                            "text": format!("instrumental: appended to '{}'", title),
                        }));
                        if cfg.scan_after_tag_write {
                            let _ = crate::wasm::trigger_navidrome_scan(cfg);
                        }
                    }
                    }
                }
            }
            // Pass 2: Verify acoustic performance — trust but verify.
            // If labeled "(Acoustic)", verify via librosa harmonic ratio and strip if not acoustic.
            // If not labeled but IS acoustic, append "(Acoustic)" to title.
            // Acoustic = high harmonic content relative to percussive (HPSS separation).
            if cfg.verify_acoustic && !cfg.essentia_url.trim().is_empty() {
                for (rel, tags) in &files {
                    if past(enrich_start, enrich_budget) { break; }
                    let title = tags.title.clone();
                    let abs = abs_of(rel);
                    let path_str = abs.to_string_lossy().to_string();
                    let cache_key = format!("acoustic:{}", path_str);

                    // Check cache first. Failed/timeout call = unknown (None):
                    // skip the label check instead of stripping on network error.
                    let is_acoustic: Option<bool> = if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
                        serde_json::from_slice::<serde_json::Value>(&v)
                            .ok()
                            .and_then(|v| v.get("isAcoustic").and_then(|v| v.as_bool()))
                    } else {
                        // Use librosa HPSS to detect acoustic character:
                        // High harmonic ratio relative to percussive = acoustic.
                        let base = cfg.essentia_url.trim().trim_end_matches('/');
                        let body = serde_json::json!({"path": path_str, "genres": false, "moods": false, "structure": false, "chroma": false, "bpm": false});
                        let req = nd_pdk::host::http::HTTPRequest {
                            method: "POST".into(),
                            url: format!("{}/instrumental-check", base),
                            headers: std::collections::HashMap::new(),
                            no_follow_redirects: false,
                            body: body.to_string().into_bytes(),
                            timeout_ms: cap_ms(enrich_start, task_budget, 8_000),
                        };
                        match nd_pdk::host::http::send(req) {
                            Ok(Some(resp)) if resp.status_code == 200 => {
                                let val: serde_json::Value = serde_json::from_slice(&resp.body).ok().unwrap_or_default();
                                let _ = crate::store::kv().set(&cache_key, resp.body);
                                // Acoustic = NOT instrumental AND has vocal content.
                                // Acoustic: has vocals but low vocal energy (0.05-0.3 range).
                                // Pure instrumental = not acoustic. High vocal energy = not acoustic.
                                val.get("isInstrumental").and_then(|v| v.as_bool()).map(|is_instr| {
                                    let vocal_ratio = val.get("vocalRatio")
                                        .and_then(|v| v.as_f64())
                                        .unwrap_or(0.0);
                                    !is_instr && vocal_ratio > 0.05 && vocal_ratio < 0.3
                                })
                            }
                            _ => None,
                        }
                    };

                    // Re-read title from file tag to avoid stale KV cache
                    // causing double-suffix on subsequent runs.
                    let current_title = crate::tags::read_tags(&abs)
                        .map(|t| t.title)
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| title.clone());
                    let current_lower = current_title.to_lowercase();
                    let current_has_label = current_lower.contains("acoustic");

                    if let Some(is_acoustic) = is_acoustic {
                    if current_has_label && !is_acoustic {
                        // Labeled acoustic but NOT actually acoustic — strip the label.
                        let stripped = crate::tags::strip_acoustic(&current_title);
                        if stripped != current_title {
                            let _ = crate::tags::write_title(&abs, &stripped);
                            crate::wasm::log_info(&format!(
                                "acoustic: stripped from '{}' -> '{}' (not truly acoustic)",
                                current_title, stripped
                            ));
                            actions.push(serde_json::json!({
                                "ts": crate::state::now_ts(),
                                "text": format!("acoustic: stripped from '{}' (not truly acoustic)", current_title),
                            }));
                            if cfg.scan_after_tag_write {
                                let _ = crate::wasm::trigger_navidrome_scan(cfg);
                            }
                        }
                    } else if is_acoustic && !current_has_label {
                        // Acoustic but not labeled — append "(Acoustic)".
                        let new_title = format!("{} (Acoustic)", current_title);
                        let _ = crate::tags::write_title(&abs, &new_title);
                        crate::wasm::log_info(&format!(
                            "acoustic: appended to '{}' -> '{}'",
                            current_title, new_title
                        ));
                        actions.push(serde_json::json!({
                            "ts": crate::state::now_ts(),
                            "text": format!("acoustic: appended to '{}'", title),
                        }));
                        if cfg.scan_after_tag_write {
                            let _ = crate::wasm::trigger_navidrome_scan(cfg);
                        }
                    }
                    }
                }
            }
            if cfg.scan_after_tag_write && remain_ms(enrich_start, task_budget) >= 6_000 {
                if let Err(e) = crate::wasm::trigger_navidrome_scan(cfg) {
                    crate::wasm::log_warn(&format!("scan trigger failed: {e}"));
                }
            }
            if cfg.notify_audiomuse_after_run
                && !cfg.audiomuse_url.trim().is_empty()
                && remain_ms(enrich_start, task_budget) >= 9_000
            {
                match crate::audiomuse::re_sync(cfg) {
                    Ok(()) => actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": "audiomuse: requested re-sync".to_string(),
                    })),
                    Err(e) => crate::wasm::log_warn(&format!("AudioMuse-AI re-sync: {e}")),
                }
            }
            if cfg.lidarr_mode == crate::config::LidarrMode::MetadataPlusRescan
                && !cfg.lidarr_url.trim().is_empty()
                && !cfg.lidarr_api_key.trim().is_empty()
                && remain_ms(enrich_start, task_budget) >= 17_000
            {
                if let Some(lidar) =
                    crate::lidarr::host_lidarr::find_album(cfg, &info.album, &info.album_artist)
                {
                    if crate::net::throttle(&format!("lidarr-refresh-{}", lidar.artist_id), 300_000) {
                        match crate::lidarr::host_lidarr::refresh_artist(cfg, lidar.artist_id) {
                            Ok(()) => {
                                crate::wasm::log_info(&format!(
                                    "Lidarr: RefreshArtist submitted for {}",
                                    lidar.artist
                                ));
                                actions.push(serde_json::json!({
                                    "ts": crate::state::now_ts(),
                                    "text": format!("lidarr: RefreshArtist for {}", lidar.artist),
                                }));
                            }
                            Err(e) => crate::wasm::log_warn(&format!("Lidarr RefreshArtist failed: {e}")),
                        }
                    }
                }
                for src_artist in &info.distinct_artists {
                    if src_artist.eq_ignore_ascii_case(&info.album_artist) {
                        continue;
                    }
                    // Each source artist costs find + refresh (≤8s each).
                    if remain_ms(enrich_start, task_budget) < 17_000 {
                        break;
                    }
                    if let Some(src_album) = crate::lidarr::host_lidarr::find_album(
                        cfg,
                        &info.album,
                        src_artist,
                    ) {
                        if crate::net::throttle(
                            &format!("lidarr-refresh-{}", src_album.artist_id),
                            300_000,
                        ) {
                            match crate::lidarr::host_lidarr::refresh_artist(cfg, src_album.artist_id) {
                                Ok(()) => {
                                    crate::wasm::log_info(&format!(
                                        "Lidarr: RefreshArtist submitted for source artist {}",
                                        src_album.artist
                                    ));
                                    actions.push(serde_json::json!({
                                        "ts": crate::state::now_ts(),
                                        "text": format!("lidarr: RefreshArtist for source artist {}", src_album.artist),
                                    }));
                                }
                                Err(e) => crate::wasm::log_warn(&format!(
                                    "Lidarr RefreshArtist (source) failed: {e}"
                                )),
                            }
                        }
                    }
                }
            }
        }
        // Write NFO at the end — after ALL metadata sources have been queried.
        // This ensures the NFO contains the most complete metadata possible.
        if cfg.write_nfo {
            write_group_nfo(cfg, &plan, &files, &fin, &sidecar_abs);
            actions.push(serde_json::json!({
                "ts": crate::state::now_ts(),
                "text": "wrote album.nfo (unified metadata)".to_string(),
            }));
        }
    }

    let mut report_text = if report_parts.is_empty() {
        format!(
            "No albums in batch {}/{}\n",
            batch_index + 1,
            batch_total.max(1)
        )
    } else {
        report_parts.join("\n")
    };
    if total_autotags > 0 {
        report_text.push_str(&format!(
            "\nauto-tag: {} track(s) tagged from MusicBrainz\n",
            total_autotags,
        ));
    }
    if total_replaygains > 0 {
        report_text.push_str(&format!("\nReplayGain: {total_replaygains} track(s) tagged\n"));
    }
    let run_id = crate::wasm::current_run_id(library_id).unwrap_or_default();
    report_text.push_str(&format!(
        "\n[rollback] Run ID: {run_id}\nTo undo everything in this run, set 'rollbackRunId' = {run_id} in the plugin settings, then run a pass.\n"
    ));
    crate::wasm::save_report(&report_text, cfg.backup_retention_days as i64);
    crate::wasm::log_info(&report_text);
    let report_envelope = serde_json::json!({
        "ts": crate::state::now_ts(),
        "mode": crate::wasm::mode_label(cfg),
        "kind": "report",
        "dryRun": false,
        "batch": { "index": batch_index, "total": batch_total },
        "runId": run_id,
        "text": report_text,
        "plans": [],
        "actions": actions,
        "libraries": [{
            "id": library_id,
            "albumsFound": groups.len(),
            "albumsToMove": 0,
            "fileMoves": 0,
            "duplicates": 0,
            "kept": 0,
            "skipped": 0
        }],
    })
    .to_string();
    crate::wasm::post_webhook(cfg, &report_envelope);
    crate::wasm::log_info(&format!(
        "ENRICH: library={} batch={}/{} autotags={} replaygains={}",
        library_id,
        batch_index + 1,
        batch_total.max(1),
        total_autotags,
        total_replaygains,
    ));
    let status_json = serde_json::json!({
        "ts": crate::state::now_ts(),
        "mode": "apply",
        "inProgress": false,
        "phase": "enrich",
        "batch": { "index": batch_index, "total": batch_total },
        "warnings": [],
        "integrations": crate::wasm::integration_health(cfg),
        "tasks": crate::wasm::task_log(),
    })
    .to_string();
    crate::wasm::post_webhook(cfg, &status_json);
    Ok(())
}

/// Background metadata refresh: enrich files in all libraries without organizing.
/// When the organize pipeline is idle, this updates tags, NFOs, ratings, artwork,
/// lyrics, genre, and other metadata for files across all Navidrome libraries.
/// No file moves or renaming — just gather and save.
pub fn meta_refresh_step(cfg: &Config, library_id: i32) -> Result<String, String> {
    let eff = crate::wasm::effective_config(cfg);
    let cfg = &eff;
    let root = lib_root(library_id)?;

    // Skip if the organize pipeline is active (walk/index/verify/group in progress).
    // meta_refresh and organize share the same queue — running meta_refresh
    // during any organize phase blocks the pipeline and stalls the run.
    let has_walk_files = crate::store::kv()
        .get(&format!("scan.walkfiles.{library_id}"))
        .ok().flatten().is_some();
    let has_walk_stack = crate::store::kv()
        .get(&format!("scan.walkv2.{library_id}"))
        .ok().flatten().is_some();
    let has_index_cursor = crate::store::kv()
        .get(&format!("scan.index_cursor.{library_id}"))
        .ok().flatten().is_some();
    let has_unverified = crate::store::kv()
        .get(&format!("scan.unverified.{library_id}"))
        .ok().flatten().is_some();
    let has_group_cursor = crate::store::kv()
        .get(&format!("scan.group_cursor.{library_id}"))
        .ok().flatten().is_some();
    if has_walk_files || has_walk_stack || has_index_cursor || has_unverified || has_group_cursor {
        return Ok("meta_refresh: deferred — organize pipeline active".into());
    }

    // Load the file list for this library. group_step copies scan.indexed.*
    // to scan.meta_files.* and deletes the original, so prefer the meta copy
    // and fall back to indexed (mid-pipeline resume, pre-group state).
    let cursor_key = format!("scan.meta_cursor.{library_id}");
    let load_list = |key: &str| -> Vec<(String, i64)> {
        crate::store::kv()
            .get(key)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default()
    };
    let mut file_list = load_list(&format!("scan.meta_files.{library_id}"));
    if file_list.is_empty() {
        file_list = load_list(&format!("scan.indexed.{library_id}"));
    }

    if file_list.is_empty() {
        return Ok("meta_refresh: no indexed files for this library".into());
    }

    // Resume from cursor.
    let mut cursor: usize = crate::store::kv()
        .get(&cursor_key)
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if cursor > file_list.len() {
        cursor = 0;
    }

    let budget = std::time::Duration::from_secs(15);
    let start = std::time::SystemTime::now();
    let mut processed = 0usize;
    let mut refreshed = 0usize;
    let mut art_written = 0usize;
    let mut nfo_written = 0usize;
    let mut seen_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut dir_files_cache: std::collections::HashMap<String, Vec<(String, TrackTags)>> =
        std::collections::HashMap::new();

    crate::wasm::log_info(&format!(
        "meta_refresh: library={} starting at {}/{}, {} files total",
        library_id, cursor, file_list.len(), file_list.len()
    ));

    // Process files with time budget.
    while cursor < file_list.len() {
        if past(start, budget) {
            break;
        }

        let (rel, _mtime) = &file_list[cursor];
        let abs = root.join(rel);

        // Check if file exists and is readable.
        if !abs.exists() {
            cursor += 1;
            continue;
        }

        // Album-level ops (once per dir per chunk, on the dir's first file),
        // BEFORE the per-file network ops: a failing endpoint in the per-file
        // block can burn the whole budget, and if the dir-op ran after it the
        // 12s entry gate would break forever on the same file (cursor stall).
        // Here the gate always sees a fresh budget on the first file of a
        // chunk, so at least one dir-op is attempted per chunk and the cursor
        // keeps moving even when sidecars are down. The 12s gate still defers
        // a mid-chunk dir (cursor not advanced → next chunk resumes there
        // with a full budget); per-file gates below are idempotent and cheap
        // to redo. The dir-op runs before the genre fetch too, so a freshly
        // written genre-less album.nfo feeds nfo_genres the same iteration.
        let dir_rel = dirname(rel).to_string();
        if !seen_dirs.contains(&dir_rel) {
            let dir_abs = root.join(&dir_rel);
            // Representative file: the dir's first file in the list (same
            // policy as plan_enrich).
            let rep_rel = file_list
                .iter()
                .map(|(r, _)| r.as_str())
                .find(|r| dirname(r) == dir_rel);
            let rep_abs = rep_rel.map(|r| root.join(r));
            let had_art = rep_abs.as_deref().map(crate::artwork::has_embedded).unwrap_or(false);
            let side_exists = dir_abs.join("cover.jpg").exists();
            let front_on = cfg.artwork_front && (cfg.embed_artwork || cfg.write_cover_jpg);
            let extras_on =
                cfg.embed_artwork && (cfg.artwork_back || cfg.artwork_cd || cfg.artwork_booklet);
            let need_embed = front_on && cfg.embed_artwork && !had_art;
            let need_side = front_on && cfg.write_cover_jpg && !side_exists;
            // Missing NFO, or one with empty genres (heals partial data from
            // an interrupted pass).
            let nfo_state = cfg.write_nfo
                && match crate::nfo::read_album_nfo(&dir_abs) {
                    None => true,
                    Some(n) => n.genres.is_empty(),
                };
            let any = nfo_state || need_embed || need_side || (extras_on && !had_art);
            if any && remain_ms(start, budget) < 12_000 {
                break;
            }
            if any {
                let dir_files = dir_files_cache.entry(dir_rel.clone()).or_insert_with(|| {
                    file_list
                        .iter()
                        .filter(|(r, _)| dirname(r) == dir_rel)
                        .filter_map(|(r, _)| {
                            crate::tags::read_tags(&root.join(r)).map(|t| (r.clone(), t))
                        })
                        .collect()
                });
                if !dir_files.is_empty() {
                    let info = crate::organizer::album_info_from_tags(dir_files);
                    let album = if info.album.is_empty() {
                        basename(&dir_rel).to_string()
                    } else {
                        info.album.clone()
                    };
                    let mbid = dir_files.iter().find_map(|(_, t)| {
                        if !t.mbid_album.trim().is_empty() {
                            Some(t.mbid_album.clone())
                        } else {
                            None
                        }
                    });

                    // Front artwork + cover.jpg sidecar — only when missing.
                    if need_embed || need_side {
                        if let Some((bytes, source)) = crate::artwork::fetch_with_fallback(
                            cfg,
                            mbid.as_deref(),
                            &info.album_artist,
                            &album,
                        ) {
                            let mut embedded = 0usize;
                            let mut sidecar = false;
                            if need_embed {
                                for (r, _) in dir_files.iter() {
                                    if past(start, budget) {
                                        break;
                                    }
                                    if crate::artwork::embed(
                                        &root.join(r),
                                        bytes.clone(),
                                        crate::artwork::ArtKind::Front,
                                    )
                                    .is_ok()
                                    {
                                        embedded += 1;
                                    }
                                }
                            }
                            if need_side
                                && crate::artwork::write_sidecar(&dir_abs, bytes.clone()).is_ok()
                            {
                                sidecar = true;
                            }
                            if embedded > 0 || sidecar {
                                crate::wasm::log_info(&format!(
                                    "meta_refresh: artwork {source} → {embedded} image(s){} — {dir_rel}",
                                    if sidecar { " + cover.jpg" } else { "" }
                                ));
                                art_written += 1;
                                refreshed += 1;
                            }
                        }
                    }
                    // Extra kinds: gated on the rep's CURRENT embedded state —
                    // after a successful front embed this must skip, because
                    // embedding Back replaces picture 0 (the front cover).
                    if let (Some(mbid), Some(rep)) = (mbid.as_deref(), rep_abs.as_deref()) {
                        if extras_on && !crate::artwork::has_embedded(rep) {
                            let mut kinds: Vec<&str> = Vec::new();
                            for (on, kind, label) in [
                                (cfg.artwork_back, crate::artwork::ArtKind::Back, "back"),
                                (cfg.artwork_cd, crate::artwork::ArtKind::Cd, "cd"),
                                (cfg.artwork_booklet, crate::artwork::ArtKind::Booklet, "booklet"),
                            ] {
                                if past(start, budget) {
                                    break;
                                }
                                if !on {
                                    continue;
                                }
                                if let Some(bytes) = crate::artwork::fetch(mbid, kind) {
                                    let mut n = 0usize;
                                    for (r, _) in dir_files.iter() {
                                        if past(start, budget) {
                                            break;
                                        }
                                        if crate::artwork::embed(&root.join(r), bytes.clone(), kind)
                                            .is_ok()
                                        {
                                            n += 1;
                                        }
                                    }
                                    if n > 0 {
                                        kinds.push(label);
                                    }
                                }
                            }
                            if !kinds.is_empty() {
                                crate::wasm::log_info(&format!(
                                    "meta_refresh: artwork embedded {} (coverartarchive) — {dir_rel}",
                                    kinds.join(", ")
                                ));
                                art_written += 1;
                                refreshed += 1;
                            }
                        }
                    }
                    // album.nfo — only when missing or genre-less; a complete
                    // NFO is never rewritten (cheap local write otherwise).
                    if nfo_state {
                        let plan = crate::organizer::GroupPlan {
                            target_dir: dir_rel.clone(),
                            ..Default::default()
                        };
                        let mut fin = std::collections::HashMap::new();
                        for (r, _) in dir_files.iter() {
                            fin.insert(r.clone(), root.join(r));
                        }
                        write_group_nfo(cfg, &plan, dir_files, &fin, &dir_abs);
                        crate::wasm::log_info(&format!(
                            "meta_refresh: wrote album.nfo — {dir_rel}"
                        ));
                        nfo_written += 1;
                        refreshed += 1;
                    }
                }
            }
            seen_dirs.insert(dir_rel);
        }

        // Verify-first: read what is actually on the file (the index KV may
        // predate tag writes) and process only the missing pieces. Refresh
        // never assumes an organize pass ran on this file before, and never
        // re-processes values already present — overwrite refresh lives in
        // organize passes, not here.
        let file_tags = crate::store::kv()
            .get(&file_key(library_id, rel))
            .ok()
            .flatten()
            .and_then(|v| parse_file_tags(&v));
        let cur = crate::tags::read_tags(&abs);

        if let Some(cur) = cur.as_ref() {
            let mut changed = false;
            let mut cur_title = cur.title.clone();

            // ReplayGain: only when the track gain tag is missing.
            if cfg.write_replaygain
                && !past(start, budget)
                && !crate::tags::replaygain_present(&abs)
            {
                if let Some((gain, peak)) = replaygain_for(cfg, &abs.to_string_lossy()) {
                    if crate::tags::write_replaygain(&abs, gain, peak, cfg.overwrite_existing_tags)
                        .unwrap_or(false)
                    {
                        changed = true;
                    }
                }
            }

            // Genre: only when the file has no genre tag. Same fallback chain
            // as plan_enrich (musicbrainz → discogs → theaudiodb → nfo).
            let mut genre_filled = !cur.genre.trim().is_empty();
            if !genre_filled && !cfg.genre_source.is_empty() && !past(start, budget) {
                let g_artist = if !cur.album_artist.trim().is_empty() {
                    cur.album_artist.clone()
                } else {
                    cur.artist.clone()
                };
                let g_mbid = if !cur.mbid_album.trim().is_empty() {
                    Some(cur.mbid_album.clone())
                } else {
                    None
                };
                let nfo_genres = if cfg.read_nfo {
                    crate::nfo::read_album_nfo(&root.join(dirname(rel)))
                        .map(|n| n.genres)
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                if let Some((genres, _source)) = fetch_genre_with_fallback(
                    cfg,
                    g_mbid.as_deref(),
                    &g_artist,
                    &cur.album,
                    &nfo_genres,
                    start,
                    budget,
                ) {
                    if crate::tags::write_genre(&abs, &genres).is_ok() {
                        genre_filled = true;
                        changed = true;
                    }
                }
            }

            // Acoustic tags (AudioMuse): only when BPM/key/mood/energy has a
            // gap. The AudioMuse 7d KV cache bounds repeat requests for values
            // the service never returns.
            if cfg.write_acoustic_tags
                && !cfg.audiomuse_url.trim().is_empty()
                && !past(start, budget)
                && remain_ms(start, budget) >= 9_000
                && crate::wasm::should_write_tags(cfg, &cur.album_artist)
                && crate::tags::acoustic_missing(&abs)
            {
                if let Some(ac) = crate::audiomuse::fetch(cfg, &cur.artist, &cur.title) {
                    match crate::audiomuse::write_tags(&abs, &ac, cfg.overwrite_existing_tags) {
                        Ok(()) => changed = true,
                        Err(e) => crate::wasm::log_warn(&format!("acoustic tags for {rel}: {e}")),
                    }
                }
            }

            // Essentia genres: only when the genre is still missing after the
            // fallback chain, with a bounded timeout so a dead sidecar cannot
            // blow the 15s chunk budget. ponytail: fills genre/mood (+BPM/key
            // when the toggles are on); mood-only gaps wait for passes.
            if cfg.genre_source == "essentia"
                && !cfg.essentia_url.trim().is_empty()
                && !genre_filled
                && !past(start, budget)
                && remain_ms(start, budget) >= 9_000
            {
                let files1 = vec![(rel.clone(), cur.clone())];
                let mut fin1 = std::collections::HashMap::new();
                fin1.insert(rel.clone(), abs.clone());
                if crate::stats::host_stats::write_essentia_genres(
                    cfg,
                    &files1,
                    &fin1,
                    cap_ms(start, budget, 8_000),
                ) > 0
                {
                    changed = true;
                }
            }

            // MBIDs verified at scan time live in the index — write them into
            // the file when the tags still lack them.
            if let Some(idx) = file_tags.as_ref() {
                if !idx.mbid_album.trim().is_empty()
                    && (cur.mbid_album.trim().is_empty() || cur.mbid_recording.trim().is_empty())
                    && !past(start, budget)
                {
                    match crate::tags::write_mbids(
                        &abs,
                        &idx.mbid_album,
                        Some(idx.mbid_recording.trim()),
                        cfg.overwrite_existing_tags,
                    ) {
                        Ok(()) => changed = true,
                        Err(e) => crate::wasm::log_warn(&format!("write mbids {rel}: {e}")),
                    }
                }
            }

            // Instrumental check — trust but verify.
            // If labeled "(Instrumental)", verify and strip if not instrumental.
            // If not labeled but IS instrumental, append "(Instrumental)".
            // cur_title tracks writes within this iteration — genre/RG/mbid/
            // acoustic writes never touch the title, so no re-read is needed.
            if cfg.verify_instrumental
                && !cfg.essentia_url.trim().is_empty()
                && !cur_title.trim().is_empty()
                && !past(start, budget)
            {
                let path_str = abs.to_string_lossy().to_string();
                let cache_key = format!("instrumental:{}", path_str);
                // Failed/timeout call = unknown (None): skip the label check
                // instead of stripping a correct "(Instrumental)" label.
                let is_instrumental: Option<bool> = if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
                    serde_json::from_slice::<serde_json::Value>(&v)
                        .ok()
                        .and_then(|v| v.get("isInstrumental").and_then(|v| v.as_bool()))
                } else {
                    let base = cfg.essentia_url.trim().trim_end_matches('/');
                    let body = serde_json::json!({"path": path_str});
                    let req = nd_pdk::host::http::HTTPRequest {
                        method: "POST".into(),
                        url: format!("{}/instrumental-check", base),
                        headers: std::collections::HashMap::new(),
                        no_follow_redirects: false,
                        body: body.to_string().into_bytes(),
                        timeout_ms: cap_ms(start, budget, 8_000),
                    };
                    match nd_pdk::host::http::send(req) {
                        Ok(Some(resp)) if resp.status_code == 200 => {
                            let val: serde_json::Value = serde_json::from_slice(&resp.body).ok().unwrap_or_default();
                            let _ = crate::store::kv().set(&cache_key, resp.body);
                            val.get("isInstrumental").and_then(|v| v.as_bool())
                        }
                        _ => None,
                    }
                };
                let has_instrumental_label = cur_title.to_lowercase().contains("instrumental");
                if let Some(is_instrumental) = is_instrumental {
                if has_instrumental_label && !is_instrumental {
                    let stripped = crate::tags::strip_instrumental(&cur_title);
                    if stripped != cur_title {
                        if crate::tags::write_title(&abs, &stripped).unwrap_or(false) {
                            changed = true;
                        }
                        cur_title = stripped;
                    }
                } else if is_instrumental && !has_instrumental_label {
                    let new_title = format!("{} (Instrumental)", cur_title);
                    if crate::tags::write_title(&abs, &new_title).unwrap_or(false) {
                        changed = true;
                    }
                    cur_title = new_title;
                }
                }
            }

            // Acoustic check — trust but verify.
            // Uses instrumental-check endpoint: acoustic = not instrumental + low vocal ratio.
            if cfg.verify_acoustic
                && !cfg.essentia_url.trim().is_empty()
                && !cur_title.trim().is_empty()
                && !past(start, budget)
            {
                let path_str = abs.to_string_lossy().to_string();
                let cache_key = format!("acoustic:{}", path_str);
                // Failed/timeout call = unknown (None): skip the label check
                // instead of stripping a correct "(Acoustic)" label.
                let is_acoustic: Option<bool> = if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
                    serde_json::from_slice::<serde_json::Value>(&v)
                        .ok()
                        .and_then(|v| v.get("isAcoustic").and_then(|v| v.as_bool()))
                } else {
                    let base = cfg.essentia_url.trim().trim_end_matches('/');
                    let body = serde_json::json!({"path": path_str, "genres": false, "moods": false, "structure": false, "chroma": false, "bpm": false});
                    let req = nd_pdk::host::http::HTTPRequest {
                        method: "POST".into(),
                        url: format!("{}/instrumental-check", base),
                        headers: std::collections::HashMap::new(),
                        no_follow_redirects: false,
                        body: body.to_string().into_bytes(),
                        timeout_ms: cap_ms(start, budget, 8_000),
                    };
                    match nd_pdk::host::http::send(req) {
                        Ok(Some(resp)) if resp.status_code == 200 => {
                            let val: serde_json::Value = serde_json::from_slice(&resp.body).ok().unwrap_or_default();
                            let _ = crate::store::kv().set(&cache_key, resp.body);
                            val.get("isInstrumental").and_then(|v| v.as_bool()).map(|is_instr| {
                                let vocal_ratio = val.get("vocalRatio")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                !is_instr && vocal_ratio > 0.05 && vocal_ratio < 0.3
                            })
                        }
                        _ => None,
                    }
                };
                let has_acoustic_label = cur_title.to_lowercase().contains("acoustic");
                if let Some(is_acoustic) = is_acoustic {
                if has_acoustic_label && !is_acoustic {
                    let stripped = crate::tags::strip_acoustic(&cur_title);
                    if stripped != cur_title {
                        if crate::tags::write_title(&abs, &stripped).unwrap_or(false) {
                            changed = true;
                        }
                        cur_title = stripped;
                    }
                } else if is_acoustic && !has_acoustic_label {
                    let new_title = format!("{} (Acoustic)", cur_title);
                    if crate::tags::write_title(&abs, &new_title).unwrap_or(false) {
                        changed = true;
                    }
                    cur_title = new_title;
                }
                }
            }

            if changed {
                refreshed += 1;
            }
        }

        cursor += 1;
        processed += 1;
    }

    // One rescan for the whole chunk (not per file) — same gate plan_enrich
    // uses, only when this chunk actually wrote something.
    if cfg.scan_after_tag_write && refreshed > 0 && remain_ms(start, budget) >= 6_000 {
        if let Err(e) = crate::wasm::trigger_navidrome_scan(cfg) {
            crate::wasm::log_warn(&format!("trigger_navidrome_scan: {e}"));
        }
    }

    // Save cursor for resume.
    if cursor < file_list.len() {
        let _ = crate::store::kv().set(&cursor_key, cursor.to_string().into_bytes());
        crate::wasm::enqueue_meta_refresh(library_id)?;
        crate::wasm::log_info(&format!(
            "meta_refresh: library={} processed={}, refreshed={}, art={}, nfo={}, cursor={}/{}, re-enqueueing",
            library_id, processed, refreshed, art_written, nfo_written, cursor, file_list.len()
        ));
        Ok(format!("meta_refresh: {processed} files processed, {refreshed} refreshed ({art_written} artwork, {nfo_written} nfo), {}/{}/{} remaining", cursor, file_list.len(), file_list.len()))
    } else {
        let _ = crate::store::kv().delete(&cursor_key);
        crate::wasm::log_info(&format!(
            "meta_refresh: library={} complete, processed={}, refreshed={}, art={}, nfo={}",
            library_id, processed, refreshed, art_written, nfo_written
        ));
        Ok(format!("meta_refresh: complete, {processed} files processed, {refreshed} refreshed ({art_written} artwork, {nfo_written} nfo)"))
    }
}

/// Auto-tag a group's tracks from the MusicBrainz release tracklist, filling
/// only genuinely-missing fields (title/artist/recording MBID/release MBID).
/// Apply mode writes (atomically); dry-run only counts what would change.
/// Returns Some(count) when any track was/would be tagged.
fn autotag_album(
    cfg: &Config,
    files: &[(String, crate::tags::TrackTags)],
    rel: &crate::musicbrainz::MbRelease,
    info: &crate::organizer::AlbumInfo,
    fin: &std::collections::HashMap<String, std::path::PathBuf>,
) -> Option<usize> {
    let tracks = crate::musicbrainz::release_tracks(&rel.release_mbid, &cfg.musicbrainz_token)?;
    if tracks.is_empty() {
        return None;
    }
    let dry = cfg.mode != Mode::Apply;
    let mut tagged = 0usize;
    for (relpath, ft) in files {
        // Match the MB track by embedded track number, else global position.
        let mbt = ft
            .track
            .and_then(|n| tracks.iter().find(|t| t.number == Some(n) || t.position == Some(n)))
            .or_else(|| tracks.first());
        let Some(mbt) = mbt else { continue };
        if ft.title.trim().is_empty() || ft.mbid_recording.trim().is_empty() {
            if dry {
                tagged += 1;
            } else {
                let Some(abs) = fin.get(relpath) else { continue };
                match crate::tags::fill_missing_from_mb(
                    abs,
                    &mbt.title,
                    &mbt.artist,
                    &mbt.recording_mbid,
                    &rel.release_mbid,
                ) {
                    Ok(true) => tagged += 1,
                    Ok(false) => {}
                    Err(e) => crate::wasm::log_warn(&format!("auto-tag {}: {e}", relpath)),
                }
            }
        }
    }
    if tagged > 0 {
        crate::wasm::log_info(&format!(
            "auto-tag: {tagged} track(s) {} from MusicBrainz ({})",
            if dry { "would be tagged" } else { "tagged" },
            info.album
        ));
        Some(tagged)
    } else {
        None
    }
}

/// Ask the acoustid sidecar's `/replaygain` endpoint for a file's loudness
/// (ffmpeg EBU R128). Returns (gain dB, peak) where gain is derived from the
/// configured reference. Cached per path for 7 days.
fn replaygain_for(cfg: &Config, abs_path: &str) -> Option<(f64, Option<f64>)> {
    if cfg.acoustid_url.trim().is_empty() {
        return None;
    }
    let cache_key = format!("rg:{:016x}", crate::state::fnv1a64(abs_path));
    if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
        if let Ok(val) = serde_json::from_slice::<Value>(&v) {
            if let Some(integrated) = val.get("integrated").and_then(|g| g.as_f64()) {
                let peak = val.get("peak").and_then(|p| p.as_f64());
                return Some((cfg.replay_gain_reference - integrated, peak));
            }
        }
    }
    let base = cfg.acoustid_url.trim_end_matches('/');
    let body = serde_json::json!({ "path": abs_path }).to_string();
    let req = host::http::HTTPRequest {
        method: "POST".into(),
        url: format!("{base}/replaygain"),
        headers: std::collections::HashMap::new(),
        no_follow_redirects: false,
        body: body.into_bytes(),
        // 30s hard extism deadline — keep well inside it.
        timeout_ms: 8_000,
    };
    match host::http::send(req) {
        Ok(Some(resp)) if resp.status_code == 200 => {
            let Ok(v) = serde_json::from_slice::<Value>(&resp.body) else {
                return None;
            };
            if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
                return None;
            }
            let rg = v.get("replaygain")?;
            let integrated = rg.get("integrated").and_then(|g| g.as_f64())?;
            let peak = rg.get("peak").and_then(|p| p.as_f64());
            let _ = crate::store::kv().set_with_ttl(
                &cache_key,
                serde_json::to_vec(&rg).unwrap_or_default(),
                7 * 24 * 3600,
            );
            Some((cfg.replay_gain_reference - integrated, peak))
        }
        Ok(Some(_)) | Ok(None) | Err(_) => None,
    }
}

/// A plain-language report block for one album group. In dry-run mode every
/// action is phrased as a simulation ("would move ...") so the report reads as
/// exactly what a real run WOULD do - the user trusts the tool by seeing the
/// work before it happens.
fn group_report(plan: &crate::organizer::GroupPlan, dry: bool) -> String {
    let mut s = String::new();
    let kind = match plan.bucket {
        crate::organizer::Bucket::Soundtrack => "Soundtrack",
        crate::organizer::Bucket::Various => "Various artists (compilation)",
        crate::organizer::Bucket::Singles => "Single / incomplete",
        crate::organizer::Bucket::Normal => "Normal album",
    };
    let verb = if dry { "would move" } else { "moved" };
    let dup_verb = if dry { "would move" } else { "moved" };
    let flag_verb = if dry { "would flag" } else { "flagged" };
    let write_verb = if dry { "would write" } else { "wrote" };
    s.push_str(&format!(
        "--- Album ({kind}){} ---\n",
        if dry { "  [DRY RUN - no changes made]" } else { "" }
    ));
    s.push_str(&format!("  Target folder: /{}\n", plan.target_dir));
    if !plan.moves.is_empty() {
        s.push_str(&format!("  Files to {} ({}):\n", verb, plan.moves.len()));
        for m in &plan.moves {
            s.push_str(&format!("    - {}  ->  /{}\n", m.from, m.to));
        }
    } else {
        s.push_str("  No files to move.\n");
    }
    if !plan.duplicates.is_empty() {
        s.push_str(&format!(
            "  Duplicates found ({}):\n",
            plan.duplicates.len()
        ));
        for dup in &plan.duplicates {
            s.push_str(&format!(
                "    - {loser}  is a duplicate of  {winner}  -> {dup_verb} to /{target}\n",
                loser = dup.loser,
                winner = dup.winner,
                dup_verb = dup_verb,
                target = dup.target
            ));
        }
    }
    if !plan.fillers.is_empty() {
        s.push_str(&format!(
            "  Filler tracks {flag_verb} (dropped by the filter proxy, files kept) ({}):\n",
            plan.fillers.len(),
            flag_verb = flag_verb
        ));
        for f in &plan.fillers {
            s.push_str(&format!("    - {f}\n"));
        }
    }
    for (path, reason) in &plan.skipped {
        s.push_str(&format!("    - {path}  --  {reason}\n"));
    }
    if dry && !plan.target_dir.is_empty() {
        s.push_str(&format!("  {write_verb} /{}/album.nfo\n", plan.target_dir));
    }
    s
}

fn dirname(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[..i],
        None => "",
    }
}
fn basename(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[i + 1..],
        None => rel,
    }
}

/// Write album.nfo at the group's target dir from the group's metadata.
fn write_group_nfo(
    cfg: &Config,
    plan: &crate::organizer::GroupPlan,
    files: &[(String, TrackTags)],
    fin: &std::collections::HashMap<String, std::path::PathBuf>,
    target_abs: &std::path::Path,
) {
    let info = crate::organizer::album_info_from_tags(files);
    let genre = if info.genre.is_empty() {
        vec![]
    } else {
        vec![info.genre.clone()]
    };
    // Fetch Apple Music album editorial notes (gated by appleMusicAlbumInfo).
    let countries = crate::apple_music::host_apple_music::parse_countries(&cfg.apple_music_countries);
    let am_description = if !cfg.apple_music_album_info {
        None
    } else {
        crate::apple_music::host_apple_music::fetch_album_info(
            cfg,
            &info.album_artist,
            &info.album,
            &countries,
        )
    };
    let description = am_description.unwrap_or_default();
    // Fetch Essentia data (structure, chords, BPM/key) for NFO from cache or sidecar.
    let essentia_data = if cfg.essentia_structure || cfg.essentia_chords || cfg.essentia_bpm {
        files
            .first()
            .and_then(|(rel, _)| fin.get(rel).cloned())
            .and_then(|abs| {
            let path_str = abs.to_string_lossy().to_string();
            let cache_key = format!("essentia:{}", path_str);
            // Try cache first.
            if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
                serde_json::from_slice::<serde_json::Value>(&v).ok()
            } else {
                // Fetch from sidecar.
                let base = cfg.essentia_url.trim().trim_end_matches('/');
                if base.is_empty() {
                    return None;
                }
                let body = serde_json::json!({
                    "path": &path_str,
                    // Request the same shape write_essentia_genres() requests:
                    // this cache entry is shared, and a genres/moods:false
                    // entry would poison the genre writer for 7 days when NFO
                    // runs before it.
                    "genres": true,
                    "moods": true,
                    "structure": cfg.essentia_structure,
                    "chroma": cfg.essentia_chords,
                    "bpm": cfg.essentia_bpm,
                });
                let mut headers = std::collections::HashMap::new();
                headers.insert("Content-Type".into(), "application/json".into());
                let req = host::http::HTTPRequest {
                    method: "POST".into(),
                    url: format!("{}/analyze", base),
                    headers,
                    no_follow_redirects: false,
                    body: body.to_string().into_bytes(),
                    timeout_ms: 20_000,
                };
                if let Ok(Some(resp)) = host::http::send(req) {
                    if resp.status_code == 200 {
                        let val = serde_json::from_slice::<serde_json::Value>(&resp.body).ok()?;
                        let _ = crate::store::kv().set_with_ttl(
                            &cache_key,
                            serde_json::to_vec(&val).unwrap_or_default(),
                            7 * 24 * 3600,
                        );
                        Some(val)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        })
    } else {
        None
    };
    // Extract NFO fields from Essentia data.
    let (bpm, key, chords, structure) = if let Some(ref data) = essentia_data {
        let bpm = data.get("bpm").and_then(|b| b.as_f64());
        let key = data.get("key").and_then(|k| k.as_str()).unwrap_or("").to_string();
        let mode = data.get("mode").and_then(|m| m.as_str()).unwrap_or("");
        let key = if key.is_empty() || mode.is_empty() {
            key
        } else {
            format!("{} {}", key, mode)
        };
        let chords: Vec<String> = data.get("chords")
            .and_then(|c| c.get("changes"))
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c.get("chord").and_then(|ch| ch.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let structure: Vec<String> = data.get("structure")
            .and_then(|s| s.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| {
                        let label = s.get("label").and_then(|l| l.as_str())?;
                        let start = s.get("start").and_then(|f| f.as_f64())?;
                        Some(format!("{}@{:.0}s", label, start))
                    })
                    .collect()
            })
            .unwrap_or_default();
        (bpm, key, chords, structure)
    } else {
        (None, String::new(), vec![], vec![])
    };
    // Fetch Discogs credits (gated by discogsCredits + discogsToken).
    let credits = if cfg.discogs_credits && !cfg.discogs_token.trim().is_empty() {
        crate::discogs::host_discogs::search_release(cfg, &info.album_artist, &info.album)
            .map(|rel| {
                crate::discogs::host_discogs::get_credits(cfg, rel.id)
                    .into_iter()
                    .map(|c| crate::nfo::NfoCredit { name: c.name, role: c.role })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };
    // Strip "(Instrumental)" from album title if verifyInstrumental is enabled.
    let album_title = if cfg.verify_instrumental {
        crate::tags::strip_instrumental(&info.album)
    } else {
        info.album.clone()
    };
    let nfo_album = crate::nfo::NfoAlbum {
        title: album_title,
        artists: info.distinct_artists.clone(),
        album_artists: if info.album_artist.is_empty() {
            vec![]
        } else {
            vec![info.album_artist.clone()]
        },
        year: info.year,
        genres: genre.clone(),
        description,
        bpm,
        key,
        chords,
        structure,
        credits,
        ..Default::default()
    };
    let path = target_abs.join("album.nfo");
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Err(e) = crate::tags::atomic_write(&path, crate::nfo::serialize_album(&nfo_album).as_bytes()) {
        crate::wasm::log_warn(&format!("write album.nfo: {e}"));
    }
    // Also write artist.nfo into the artist folder (parent of the album dir)
    // with the artist name + genres + similar artists. Kodi reads it there.
    if let Some(artist_dir) = target_abs.parent().filter(|_| !plan.target_dir.is_empty()) {
        if !info.album_artist.trim().is_empty() {
            // Fetch Apple Music similar artists (gated by appleMusicSimilarArtists).
            let similar_artists = if cfg.apple_music_similar_artists {
                crate::apple_music::host_apple_music::fetch_similar_artists(
                    cfg,
                    &info.album_artist,
                    &countries,
                )
            } else {
                None
            }
            .unwrap_or_default();
            // Fetch Apple Music artist bio (gated by appleMusicArtistBios).
            let artist_bio = if cfg.apple_music_artist_bios {
                crate::apple_music::host_apple_music::fetch_artist_bio(
                    cfg,
                    &info.album_artist,
                    &countries,
                )
            } else {
                None
            }
            .unwrap_or_default();
            // Fetch Apple Music artist image (gated by appleMusicArtistImages).
            let artist_image = if cfg.apple_music_artist_images {
                crate::apple_music::host_apple_music::fetch_artist_image(
                    cfg,
                    &info.album_artist,
                    &countries,
                )
            } else {
                None
            }
            .unwrap_or_default();
            // Read existing artist.nfo if present to preserve other fields.
            let a_path = artist_dir.join("artist.nfo");
            let existing_nfo = if let Ok(xml) = std::fs::read_to_string(&a_path) {
                crate::nfo::parse_artist_nfo(&xml)
            } else {
                None
            };
            let nfo_artist = crate::nfo::NfoArtist {
                name: info.album_artist.clone(),
                genres: genre.clone(),
                similar_artists,
                biography: artist_bio,
                thumb: artist_image,
                ..existing_nfo.unwrap_or_default()
            };
            if let Some(p) = a_path.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            if let Err(e) = crate::tags::atomic_write(&a_path, crate::nfo::serialize_artist(&nfo_artist).as_bytes()) {
                crate::wasm::log_warn(&format!("write artist.nfo: {e}"));
            }
        }
    }
}

/// Write acoustic tags (BPM/key/mood/energy) for each moved file from the
/// AudioMuse-AI instance. Best-effort - never fails the run. Returns how many
/// tracks got tags.
fn write_acoustic_tags_for(
    cfg: &Config,
    plan: &crate::organizer::GroupPlan,
    files: &[(String, TrackTags)],
    fin: &std::collections::HashMap<String, std::path::PathBuf>,
    start: std::time::SystemTime,
    budget: std::time::Duration,
) -> usize {
    let mut written = 0usize;
    for (rel, t) in files {
        // Each audiomuse fetch costs up to ~8s against the task's 30s cap.
        if remain_ms(start, budget) < 9_000 {
            break;
        }
        if !crate::wasm::should_write_tags(cfg, &t.album_artist) {
            continue;
        }
        let Some(final_path) = fin.get(rel) else { continue };
        let Some(ac) = crate::audiomuse::fetch(cfg, &t.artist, &t.title) else {
            continue;
        };
        match crate::audiomuse::write_tags(final_path, &ac, cfg.overwrite_existing_tags) {
            Ok(()) => written += 1,
            Err(e) => crate::wasm::log_warn(&format!("acoustic tags for {}: {e}", rel)),
        }
    }
    if written > 0 {
        crate::wasm::log_info(&format!(
            "audiomuse: wrote acoustic tags for {written} track(s) in {}",
            plan.target_dir
        ));
    }
    written
}

/// Fetch lyrics (LRCLIB) for each moved file and write an .lrc / .txt sidecar at
/// its final location. Best-effort - never fails the run. Returns how many
/// sidecars were written.
fn download_lyrics_for(
    plan: &crate::organizer::GroupPlan,
    files: &[(String, TrackTags)],
    fin: &std::collections::HashMap<String, std::path::PathBuf>,
    format: &str,
    lyrics_source: &str,
    cfg: &crate::config::Config,
    start: std::time::SystemTime,
    budget: std::time::Duration,
) -> usize {
    let mut written = 0usize;
    for (rel, t) in files {
        // Each LRCLIB/Genius fetch costs up to ~8s against the task's 30s cap.
        if remain_ms(start, budget) < 9_000 {
            break;
        }
        let Some(final_path) = fin.get(rel) else { continue };
        // Try LRCLIB first (always), then Genius as fallback.
        let lyr = crate::lyrics::fetch(&t.artist, &t.title, &t.album, 0)
            .or_else(|| {
                if lyrics_source == "genius" && !cfg.genius_token.is_empty() {
                    crate::genius::host_genius::search_song(cfg, &t.artist, &t.title)
                        .and_then(|song| crate::genius::host_genius::get_lyrics(cfg, song.id))
                        .map(|text| crate::lyrics::Lyrics { synced: None, plain: Some(text) })
                } else {
                    None
                }
            });
        let Some(lyr) = lyr else { continue };
        match crate::lyrics::write_sidecar(final_path, &lyr, format) {
            Ok(()) => written += 1,
            Err(e) => crate::wasm::log_warn(&format!("lyrics for {}: {e}", rel)),
        }
    }
    if written > 0 {
        crate::wasm::log_info(&format!(
            "lyrics: wrote {written} sidecar(s) for {}",
            plan.target_dir
        ));
    }
    written
}

/// Genre fallback chain: try selected source, then others in order.
/// Returns (genres, source_name) on success. Network results are cached in
/// KV for 7 days keyed by lowercase artist|album.
fn fetch_genre_with_fallback(
    cfg: &crate::config::Config,
    mbid: Option<&str>,
    artist: &str,
    album: &str,
    nfo_genres: &[String],
    start: std::time::SystemTime,
    budget: std::time::Duration,
) -> Option<(Vec<String>, String)> {
    let cache_key = format!(
        "genres:{}|{}",
        artist.trim().to_lowercase(),
        album.trim().to_lowercase()
    );
    if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
        if let Ok(val) = serde_json::from_slice::<(Vec<String>, String)>(&v) {
            return Some(val);
        }
    }
    let sources = match cfg.genre_source.as_str() {
        "musicbrainz" => vec!["musicbrainz", "discogs", "theaudiodb", "essentia", "nfo"],
        "discogs" => vec!["discogs", "musicbrainz", "theaudiodb", "essentia", "nfo"],
        "theaudiodb" => vec!["theaudiodb", "musicbrainz", "discogs", "essentia", "nfo"],
        "essentia" => vec!["essentia", "musicbrainz", "discogs", "theaudiodb", "nfo"],
        "nfo" => vec!["nfo", "musicbrainz", "discogs", "theaudiodb", "essentia"],
        _ => vec!["musicbrainz", "discogs", "theaudiodb", "essentia", "nfo"],
    };
    for source in sources {
        // Network sources cost up to 8s each (module timeout) — never start
        // one with less than 9s of the task's 30s deadline left. nfo/essentia
        // are local/skipped here.
        if matches!(source, "musicbrainz" | "discogs" | "theaudiodb")
            && remain_ms(start, budget) < 9_000
        {
            break;
        }
        let hit = match source {
            "musicbrainz" => mbid
                .and_then(|m| crate::musicbrainz::fetch_genres(m, &cfg.musicbrainz_token))
                .map(|g| (g, "musicbrainz".to_string())),
            "discogs" if !cfg.discogs_token.is_empty() => crate::discogs::host_discogs::fetch_genres(cfg, artist, album)
                .map(|g| (g, "discogs".to_string())),
            "theaudiodb" if !cfg.theaudiodb_key.is_empty() => crate::theaudiodb::host_theaudiodb::fetch_genres(cfg, artist, album)
                .map(|g| (g, "theaudiodb".to_string())),
            // Essentia genres are written by write_essentia_genres() separately.
            "essentia" => None,
            "nfo" if !nfo_genres.is_empty() => Some((nfo_genres.to_vec(), "nfo".to_string())),
            _ => None,
        };
        if let Some((genres, src)) = hit {
            if src != "nfo" {
                if let Ok(bytes) = serde_json::to_vec(&(genres.clone(), src.clone())) {
                    let _ = crate::store::kv().set_with_ttl(&cache_key, bytes, 7 * 24 * 3600);
                }
            }
            return Some((genres, src));
        }
    }
    None
}





