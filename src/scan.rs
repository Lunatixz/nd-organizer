// Full-library scan: a chunked, resumable walk that builds a metadata index,
// then groups files into albums by their TAGS (not folders) and plans/applies
// the result. Only available on the wasm target (uses host services).

use std::collections::HashMap;
use std::path::Path;

use nd_pdk::host;
use serde_json::{json, Value};

use crate::config::{Config, Mode};
use crate::organizer::is_audio;
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
    let scan_start = std::time::Instant::now();
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
            if scan_start.elapsed() >= time_budget {
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
    let status = serde_json::json!({
        "ts": crate::state::now_ts(),
        "mode": crate::wasm::mode_label(cfg),
        "inProgress": true,
        "phase": "scan",
        "filesScanned": total,
        "chunkSize": chunk,
        "currentFile": current_file,
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

    // If no delta exists yet, this is the first chunk — clear stale file lists.
    if crate::store::kv().get(&delta_key).ok().flatten().is_none() {
        let _ = crate::store::kv().delete(&files_key);
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

    let scan_start = std::time::Instant::now();
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
            if scan_start.elapsed() >= time_budget {
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
                        files.push((rel, mtime));
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
                if scan_start.elapsed() >= time_budget || files.len() >= entries_per_chunk {
                    hit_limit = true;
                    break;
                }
            }
        }
        for sub in subdirs.into_iter().rev() {
            // Only push subdirs not already visited or queued.
            if !visited_dirs.contains(&sub) && !stack.contains(&sub) {
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
            let _ = crate::store::kv().set(&format!("scan.walkcount.{library_id}"), (file_count + delta.len()).to_string().into_bytes());
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
    let mut files: Vec<(String, i64)> = crate::store::kv()
        .get(&files_key)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or_default();

    if files.is_empty() {
        // Nothing to index — transition to group phase.
        let _ = crate::store::kv().delete(&files_key);
        let _ = crate::store::kv().set(&format!("scan.donev2.{library_id}"), b"1".to_vec());
        crate::wasm::enqueue_group_task(library_id)?;
        post_scan_status(cfg, library_id, 0, "index complete");
        return Ok((ScanOutcome::Done, 0));
    }

    let files_per_task = cfg.files_per_scan_task.max(1);
    let cap = cfg.max_scan_entries;
    let pass_count: usize = crate::store::kv()
        .get(&format!("scan.pass.{library_id}"))
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8_lossy(&v).parse().ok())
        .unwrap_or(0);

    let scan_start = std::time::Instant::now();
    // Conservative 15s budget — Navidrome's WASM scheduler kills at ~27s,
    // not the full 30s. This leaves margin for tag I/O overhead.
    let time_budget = std::time::Duration::from_secs(15);
    let mut processed = 0usize;
    let mut skipped = 0usize;
    let mut last_rel = String::new();

    crate::wasm::log_info(&format!(
        "index_step: starting chunk, files_remaining={}, pass_count={}",
        files.len(), pass_count
    ));

    // Process files from the front of the list.
    // Skip files whose mtime hasn't changed (already indexed).
    let mut i = 0;
    while i < files.len() {
        if scan_start.elapsed() >= time_budget {
            break;
        }
        if cap > 0 && pass_count + processed >= cap {
            break;
        }
        if processed >= files_per_task {
            break;
        }
        let (rel, stored_mtime) = &files[i];
        last_rel = rel.clone();
        let abs = root.join(rel);
        let current_mtime = file_mtime(&abs);
        if current_mtime == *stored_mtime {
            skipped += 1;
            i += 1;
            continue;
        }
        let did_work = index_file(cfg, library_id, rel, &abs)?;
        if did_work {
            processed += 1;
        } else {
            skipped += 1;
        }
        i += 1;
        if scan_start.elapsed() >= time_budget {
            break;
        }
    }

    // Save the complete file list to indexed key for group_step.
    // Don't drain — the full list is always saved.
    let indexed_key = format!("scan.indexed.{library_id}");
    crate::store::kv()
        .set(&indexed_key, serde_json::to_vec(&files).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    let _ = crate::store::kv().set(
        &format!("scan.pass.{library_id}"),
        (pass_count + processed).to_string().into_bytes(),
    );

    crate::wasm::log_info(&format!(
        "index_step: chunk done, processed={}, skipped={}, files_total={}",
        processed, skipped, files.len()
    ));

    let capped = cap > 0 && pass_count + processed >= cap;

    if capped {
        post_scan_status(cfg, library_id, processed, &last_rel);
        Ok((ScanOutcome::Paused, processed))
    } else {
        // All files done — save indexed key, clean up, enqueue verify.
        post_scan_status(cfg, library_id, processed, &format!(
            "indexing complete: {} files indexed, {} skipped (unchanged)",
            processed, skipped
        ));
        let _ = crate::store::kv().delete(&files_key);
        crate::wasm::enqueue_verify_task(library_id)?;
        post_scan_status(cfg, library_id, processed, &last_rel);
        Ok((ScanOutcome::Done, processed))
    }
}

/// Phase 3: Verify file identities via AcoustID sidecar batch processing.
/// Loads unverified files, sends them to the acoustid sidecar in batches,
/// saves verified results to KV, and enqueues group task when complete.
pub fn verify_step(
    cfg: &Config,
    library_id: i32,
) -> Result<(ScanOutcome, usize), String> {
    let root = lib_root(library_id)?;
    post_phase_status(cfg, library_id, "verify");

    let indexed_key = format!("scan.indexed.{library_id}");
    let unverified_key = format!("scan.unverified.{library_id}");

    // Load cached unverified list, or recompute from indexed key.
    let unverified: Vec<(String, i64)> = crate::store::kv()
        .get(&unverified_key)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or_else(|| {
            // Recompute: load full indexed list, batch-check which are unverified.
            crate::wasm::log_info("verify_step: recomputing unverified list from indexed key");
            let file_list: Vec<(String, i64)> = crate::store::kv()
                .get(&indexed_key)
                .ok()
                .flatten()
                .and_then(|v| serde_json::from_slice(&v).ok())
                .unwrap_or_default();

            let file_keys: Vec<String> = file_list.iter().map(|(rel, _)| file_key(library_id, rel)).collect();
            let batch_size_kv = 500;
            let mut verified_set: std::collections::HashSet<String> = std::collections::HashSet::new();
            for chunk in file_keys.chunks(batch_size_kv) {
                if let Ok(entries) = crate::store::kv().get_many(chunk.to_vec()) {
                    for (k, v) in entries {
                        if let Ok(val) = serde_json::from_slice::<Value>(&v) {
                            if let Some(tags) = val.get("tags") {
                                if !tags.is_null() {
                                    if let Ok(t) = serde_json::from_value::<TrackTags>(tags.clone()) {
                                        if !t.mbid_album.is_empty() {
                                            verified_set.insert(k);
                                            continue;
                                        }
                                    }
                                    if tags.get("_acoustid_checked").and_then(|v| v.as_bool()).unwrap_or(false) {
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
            // Cache for next run.
            let _ = crate::store::kv().set(&unverified_key, serde_json::to_vec(&list).unwrap_or_default());
            list
        });

    if unverified.is_empty() {
        crate::wasm::log_info("verify_step: all files verified, transitioning to group");
        let _ = crate::store::kv().delete(&indexed_key);
        let _ = crate::store::kv().delete(&unverified_key);
        let _ = crate::store::kv().set(&format!("scan.donev2.{library_id}"), b"1".to_vec());
        crate::wasm::enqueue_group_task(library_id)?;
        post_scan_status(cfg, library_id, 0, "verification complete");
        return Ok((ScanOutcome::Done, 0));
    }

    // Process in batches of 50 via acoustid sidecar.
    let batch_size = 50;
    let batch: Vec<&(String, i64)> = unverified.iter().take(batch_size).collect();
    let batch_files: Vec<serde_json::Value> = batch.iter().map(|(rel, mtime)| {
        let abs = root.join(rel);
        serde_json::json!({"path": abs.to_string_lossy(), "mtime": mtime})
    }).collect();

    let acoustid_url = cfg.acoustid_url.trim().trim_end_matches('/');
    let body = serde_json::json!({
        "files": batch_files,
        "acoustidApiKey": cfg.acoustid_api_key,
    });

    crate::wasm::log_info(&format!(
        "verify_step: sending {} files to acoustid sidecar",
        batch.len()
    ));

    let req = host::http::HTTPRequest {
        method: "POST".into(),
        url: format!("{}/batch", acoustid_url),
        headers: std::collections::HashMap::new(),
        no_follow_redirects: false,
        body: body.to_string().into_bytes(),
        timeout_ms: 300_000, // 5 minutes for batch
    };

    match host::http::send(req) {
        Ok(Some(resp)) if resp.status_code == 200 => {
            let result: serde_json::Value = serde_json::from_slice(&resp.body)
                .map_err(|e| format!("bad batch response: {e}"))?;

            // Save verified results to individual KV entries.
            if let Some(results) = result.get("results").and_then(|r| r.as_array()) {
                for r in results {
                    if let Some(path) = r.get("path").and_then(|p| p.as_str()) {
                        // Update the file's tags in KV with the resolved MBIDs.
                        let rel = path.trim_start_matches(&root.to_string_lossy().to_string())
                            .trim_start_matches('/');
                        let key = file_key(library_id, rel);
                        if let Ok(Some(v)) = crate::store::kv().get(&key) {
                            if let Ok(mut val) = serde_json::from_slice::<Value>(&v) {
                                if let Some(tags) = val.get_mut("tags") {
                                    if let Some(t) = tags.as_object_mut() {
                                        if let Some(matches) = r.get("matches").and_then(|m| m.as_array()) {
                                            if let Some(top) = matches.first() {
                                                let album_mbid = top.get("releaseGroups")
                                                    .and_then(|rg| rg.as_array())
                                                    .and_then(|a| a.first())
                                                    .and_then(|g| g.get("id"))
                                                    .and_then(|id| id.as_str())
                                                    .map(String::from)
                                                    .unwrap_or_default();
                                                let recording_mbid = top.get("id")
                                                    .and_then(|id| id.as_str())
                                                    .map(String::from)
                                                    .unwrap_or_default();
                                                t.insert("mbid_album".into(), serde_json::Value::String(album_mbid));
                                                t.insert("mbid_recording".into(), serde_json::Value::String(recording_mbid));
                                            }
                                        }
                                        // Mark file as checked even if no match — prevents re-sending.
                                        t.insert("_acoustid_checked".into(), serde_json::Value::Bool(true));
                                    }
                                }
                                let _ = crate::store::kv().set(&key, val.to_string().into_bytes());
                            }
                        }
                    }
                }
            }

            let processed = result.get("processed").and_then(|p| p.as_u64()).unwrap_or(0) as usize;
            crate::wasm::log_info(&format!(
                "verify_step: processed {}/{} files, {} remaining",
                processed, batch.len(), unverified.len() - processed
            ));

            // Remove processed files from the cached unverified list.
            let remaining: Vec<(String, i64)> = unverified[processed..].to_vec();
            if remaining.is_empty() {
                let _ = crate::store::kv().delete(&unverified_key);
            } else {
                let _ = crate::store::kv().set(&unverified_key, serde_json::to_vec(&remaining).unwrap_or_default());
            }

            if !remaining.is_empty() {
                // More files to verify — re-enqueue.
                crate::wasm::enqueue_verify_task(library_id)?;
                post_scan_status(cfg, library_id, processed, &format!(
                    "verifying... {}/{} files verified", processed, unverified.len()
                ));
                Ok((ScanOutcome::More, processed))
            } else {
                // All files verified — transition to group.
                let _ = crate::store::kv().delete(&indexed_key);
                let _ = crate::store::kv().set(&format!("scan.donev2.{library_id}"), b"1".to_vec());
                crate::wasm::enqueue_group_task(library_id)?;
                post_scan_status(cfg, library_id, processed, "verification complete");
                Ok((ScanOutcome::Done, processed))
            }
        }
        _ => {
            crate::wasm::log_warn("verify_step: acoustid sidecar unavailable, skipping verification");
            // Skip verification — group with existing tags.
            let _ = crate::store::kv().delete(&indexed_key);
            let _ = crate::store::kv().delete(&unverified_key);
            let _ = crate::store::kv().set(&format!("scan.donev2.{library_id}"), b"1".to_vec());
            crate::wasm::enqueue_group_task(library_id)?;
            post_scan_status(cfg, library_id, 0, "verification skipped (sidecar offline)");
            Ok((ScanOutcome::Done, 0))
        }
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
    for prefix in ["scan.filev2.", "scan.stackv2.", "star.tally.", "star.pub.", "stat.play.", "stat.skip."] {
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

    // AcoustID verification is now handled by verify_step (sidecar batch).
    // The group_step just loads verified files from KV.
    // Uses a cursor to resume across multiple task invocations.

    let indexed_key = format!("scan.indexed.{library_id}");
    let cursor_key = format!("scan.group_cursor.{library_id}");
    let entries_key = format!("scan.group_entries.{library_id}");

    // Load or initialize file list from indexed key (only on first chunk).
    let mut cursor: usize = crate::store::kv()
        .get(&cursor_key)
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let file_list: Vec<(String, i64)> = if cursor == 0 {
        // First chunk — load the full file list and save entries key.
        let list: Vec<(String, i64)> = crate::store::kv()
            .get(&indexed_key)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default();
        // Save the file list size for progress tracking.
        let _ = crate::store::kv().set(&entries_key, list.len().to_string().into_bytes());
        list
    } else {
        // Resuming — we don't need the full list, just continue from cursor.
        // Load the list length to know when we're done.
        let total: usize = crate::store::kv()
            .get(&entries_key)
            .ok()
            .flatten()
            .and_then(|v| String::from_utf8(v).ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if total == 0 {
            // Entries key missing — something went wrong, restart.
            cursor = 0;
            Vec::new()
        } else {
            crate::wasm::log_info(&format!(
                "group_step: resuming from cursor {cursor}/{total}"
            ));
            // We need the file list to continue. Reload it.
            crate::store::kv()
                .get(&indexed_key)
                .ok()
                .flatten()
                .and_then(|v| serde_json::from_slice(&v).ok())
                .unwrap_or_default()
        }
    };

    if file_list.is_empty() {
        // No files — check if we already grouped.
        if let Ok(Some(v)) = crate::store::kv().get(&format!("scan.donev2.{library_id}")) {
            if v == b"1" {
                crate::wasm::log_info("group_step: already done, skipping");
                return Ok((0, 0));
            }
        }
        crate::wasm::log_info("group_step: no files to group");
        return Ok((0, 0));
    }

    crate::wasm::log_info(&format!(
        "group_step: reading tags from cursor {}/{}...",
        cursor, file_list.len()
    ));

    // Read tags from individual KV entries in time-budgeted batches.
    let scan_start = std::time::Instant::now();
    let time_budget = std::time::Duration::from_secs(15);
    let mut entries: Vec<(String, TrackTags)> = Vec::new();
    let batch_size = 500;
    let mut hit_budget = false;

    for chunk in file_list[cursor..].chunks(batch_size) {
        if scan_start.elapsed() >= time_budget {
            hit_budget = true;
            break;
        }
        let keys: Vec<String> = chunk
            .iter()
            .map(|(rel, _)| file_key(library_id, rel))
            .collect();
        if let Ok(values) = crate::store::kv().get_many(keys) {
            for (rel, _mtime) in chunk {
                let key = file_key(library_id, rel);
                if let Some(v) = values.get(&key) {
                    if let Ok(val) = serde_json::from_slice::<Value>(v) {
                        if let Some(tags) = val.get("tags") {
                            if !tags.is_null() {
                                if let Ok(t) = serde_json::from_value::<TrackTags>(tags.clone()) {
                                    entries.push((rel.clone(), t));
                                }
                            }
                        }
                    }
                }
            }
        }
        cursor += chunk.len();
    }

    // If we hit the time budget, save cursor and re-enqueue for next chunk.
    if hit_budget {
        let _ = crate::store::kv().set(&cursor_key, cursor.to_string().into_bytes());
        // Accumulate entries into the entries KV so we don't lose progress.
        let mut prev_entries: Vec<(String, TrackTags)> = crate::store::kv()
            .get(&format!("scan.group_entries.{library_id}"))
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default();
        prev_entries.extend(entries);
        let _ = crate::store::kv().set(
            &format!("scan.group_entries.{library_id}"),
            serde_json::to_vec(&prev_entries).unwrap_or_default(),
        );
        crate::wasm::log_info(&format!(
            "group_step: time budget hit at {cursor}/{}, saved {} entries, re-enqueueing",
            file_list.len(), prev_entries.len()
        ));
        crate::wasm::enqueue_group_task(library_id)?;
        return Ok((0, 0));
    }

    // All files processed — load any accumulated entries from previous chunks.
    let mut all_entries: Vec<(String, TrackTags)> = crate::store::kv()
        .get(&format!("scan.group_entries.{library_id}"))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or_default();
    all_entries.extend(entries);
    let _ = crate::store::kv().delete(&cursor_key);
    let _ = crate::store::kv().delete(&entries_key);
    let _ = crate::store::kv().delete(&format!("scan.group_entries.{library_id}"));

    let total_files = all_entries.len();
    let verified: Vec<(String, TrackTags)> = all_entries;

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

    // Report files across the library that share an audio fingerprint (size +
    // content sample) - possible duplicates. Report-only, nothing moves.
    if cfg.detect_duplicates {
        report_cross_duplicates(cfg, &real_root, &verified);
    }
    // Essentia fingerprint-based duplicate/cover detection (enhanced detection).
    if cfg.essentia_fingerprint && !cfg.essentia_url.trim().is_empty() {
        report_essentia_duplicates(cfg, &real_root, &verified);
    }

    let groups = crate::organizer::group_entries(&verified);
    let groups = apply_album_budget(cfg, groups);
    if cfg.star_tally_enabled {
        let pruned = crate::stats::host_stats::prune_star_tallies();
        if pruned > 0 {
            crate::wasm::log_info(&format!("star: pruned {pruned} orphaned tallie(s)"));
        }
    }
    let enqueued = crate::wasm::enqueue_plan_tasks(cfg, library_id, groups)?;
    crate::wasm::log_info(&format!(
        "group_step: grouped {} files into {} plan tasks (enqueued)",
        total_files, enqueued
    ));
    // After the plan/apply work runs, sweep for folders left with no audio
    // (images/nfo/lyrics/misc only) - gated by cleanupNoAudioFolders.
    if cfg.cleanup_no_audio_folders {
        crate::wasm::enqueue_cleanup_task(library_id)?;
    }
    Ok((enqueued, total_files))
}

/// Report files that share an audio fingerprint across the whole library
/// (possible duplicates). Only samples files whose SIZE collides, so exact
/// copies are found without reading every file fully. Report-only.
fn report_cross_duplicates(
    cfg: &Config,
    root: &str,
    verified: &[(String, crate::tags::TrackTags)],
) {
    use std::collections::HashMap;
    let mut by_size: HashMap<u64, Vec<String>> = HashMap::new();
    for (rel, _) in verified {
        if let Ok(md) = std::fs::metadata(std::path::Path::new(root).join(rel)) {
            by_size.entry(md.len()).or_default().push(rel.clone());
        }
    }
    let mut fp_map: HashMap<u64, Vec<String>> = HashMap::new();
    for (_, rels) in by_size.iter().filter(|(_, v)| v.len() > 1) {
        for rel in rels {
            if let Some(fp) = content_fingerprint(&std::path::Path::new(root).join(rel)) {
                fp_map.entry(fp).or_default().push(rel.clone());
            }
        }
    }
    let dupes: Vec<&Vec<String>> = fp_map.values().filter(|v| v.len() > 1).collect();
    if dupes.is_empty() {
        return;
    }
    let mut summary = String::from("nd-organizer: possible duplicate audio files:\n");
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
) {
    let base = cfg.essentia_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return;
    }
    let groups = crate::organizer::group_entries(verified);
    let mut covers = Vec::new();
    let mut dupes = Vec::new();
    for group in &groups {
        // Compare each pair within the group (cap at 20 files to avoid O(n^2) explosion).
        let limit = group.len().min(20);
        for i in 0..limit {
            for j in (i + 1)..limit {
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
                    timeout_ms: 30_000,
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
        }
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
    let mut deleted = 0usize;
    walk_cleanup(&root, &root, cfg, dry, &mut deleted);
    crate::wasm::log_info(&format!(
        "cleanup: {} no-audio folder(s) {}",
        deleted,
        if dry { "would be deleted (dry-run)" } else { "deleted" }
    ));
    Ok(deleted)
}

/// Bottom-up walk. Returns true when the subtree (still) contains audio - a
/// deleted no-audio child returns false so empty-of-audio parents cascade up.
fn walk_cleanup(
    dir: &std::path::Path,
    root: &std::path::Path,
    cfg: &Config,
    dry: bool,
    deleted: &mut usize,
) -> bool {
    let rel = dir
        .strip_prefix(root)
        .unwrap_or(dir)
        .to_string_lossy()
        .replace('\\', "/");
    if crate::organizer::is_excluded(&rel, &cfg.exclude_paths) {
        return true; // never inspect or delete inside excluded paths
    }
    let mut has_audio = false;
    let mut children: Vec<std::path::PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if cfg.skip_hidden_files && name.starts_with('.') {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            children.push(e.path());
        } else if ft.is_file() && crate::organizer::is_audio(&name) {
            has_audio = true;
        }
    }
    for c in &children {
        if walk_cleanup(c, root, cfg, dry, deleted) {
            has_audio = true;
        }
    }
    if !has_audio && dir != root {
        if dry {
            crate::wasm::log_info(&format!(
                "cleanup: would delete {} (no audio files)",
                dir.display()
            ));
            *deleted += 1;
        } else if std::fs::remove_dir_all(dir).is_ok() {
            crate::wasm::log_info(&format!("cleanup: deleted {} (no audio files)", dir.display()));
            *deleted += 1;
        }
    }
    has_audio
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

    for group in groups {
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
        let info = crate::organizer::album_info_from_tags(&files);
        let mb_release = if cfg.classify_from_mb
            && cfg.primary_source == crate::config::PrimarySource::MusicBrainz
        {
            crate::musicbrainz::lookup(&info.album_artist, &info.album, &cfg.musicbrainz_token)
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
        if cfg.lidarr_force_search_incomplete && !cfg.lidarr_url.trim().is_empty() {
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
                match crate::lidarr::host_lidarr::force_search(cfg, album_id) {
                    Ok(()) => crate::wasm::log_info("Lidarr AlbumSearch submitted"),
                    Err(e) => crate::wasm::log_warn(&format!("Lidarr AlbumSearch failed: {e}")),
                }
            }
        }
        let plan = crate::organizer::build_group_plan(&root, cfg, &files, &folder_hint, &mb_type);
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
            if cfg.scan_after_album {
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
            if cfg.write_nfo {
                write_group_nfo(&root, cfg, &plan, &files);
                actions.push(serde_json::json!({
                    "ts": crate::state::now_ts(),
                    "text": "wrote album.nfo".to_string(),
                }));
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
    crate::wasm::log_info(&format!(
        "enrich_step: batch {}/{}, {} album(s), plan: [{}]",
        batch_index + 1, batch_total, groups.len(), enrichments.join(", ")
    ));
    let mut report_parts = Vec::new();
    let mut actions: Vec<serde_json::Value> = Vec::new();
    let mut total_autotags = 0usize;
    let mut total_replaygains = 0usize;

    for group in groups {
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
        let info = crate::organizer::album_info_from_tags(&files);
        let mb_release = if cfg.classify_from_mb
            && cfg.primary_source == crate::config::PrimarySource::MusicBrainz
        {
            crate::musicbrainz::lookup(&info.album_artist, &info.album, &cfg.musicbrainz_token)
        } else {
            None
        };
        let plan = crate::organizer::build_group_plan(&root, cfg, &files, &folder_hint, &mb_release.as_ref().map(|r| {
            if r.primary_type == "Soundtrack" { "Soundtrack".to_string() }
            else if r.secondary_types.iter().any(|t| t.eq_ignore_ascii_case("compilation") || t.eq_ignore_ascii_case("live")) || r.primary_type == "Compilation" { "Compilation".to_string() }
            else if r.primary_type == "Single" || r.primary_type == "EP" { "Single".to_string() }
            else { String::new() }
        }).unwrap_or_default());
        report_parts.push(group_report(&plan, false));

        if !plan.moves.is_empty() {
            if cfg.auto_tag_from_mb {
                if let Some(rel) = &mb_release {
                    if let Some(tagged) = autotag_album(cfg, &root, &files, rel, &info) {
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
                    let abs = root.join(rel);
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
            if cfg.embed_artwork || cfg.write_cover_jpg {
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
                    let dir = root.join(&plan.target_dir);
                    let mut embedded = 0usize;
                    let mut sidecar = false;
                    if cfg.embed_artwork {
                        let first = files.first().map(|(r, _)| root.join(r)).unwrap_or_default();
                        if cfg.overwrite_art || !crate::artwork::has_embedded(&first) {
                            for (rel, _) in files.iter() {
                                let path = root.join(rel);
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
            if cfg.lyrics_source == "lrclib" || cfg.lyrics_source == "genius" {
                let n = download_lyrics_for(&root, &plan, &files, cfg.lyrics_format.as_str(), &cfg.lyrics_source, cfg);
                if n > 0 {
                    actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": format!("lyrics: fetched {n} sidecar(s)"),
                    }));
                }
            }
            if !cfg.genre_source.is_empty() {
                let mbid = files.iter().find_map(|(_, t)| {
                    if !t.mbid_album.trim().is_empty() {
                        Some(t.mbid_album.clone())
                    } else {
                        None
                    }
                });
                let nfo_genres = if cfg.read_nfo {
                    crate::nfo::read_album_nfo(&root.join(&plan.target_dir))
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
                ) {
                    for (rel, _tags) in files.iter() {
                        let path = root.join(rel);
                        let _ = crate::tags::write_genre(&path, &genres);
                    }
                    actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": format!("genre: {source} → {} tag(s)", genres.len()),
                    }));
                }
            }
            if cfg.write_acoustic_tags && !cfg.audiomuse_url.trim().is_empty() {
                let n = write_acoustic_tags_for(cfg, &root, &plan, &files);
                if n > 0 {
                    actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": format!("acoustic tags: BPM/key/mood for {n} track(s)"),
                    }));
                }
            }
            if cfg.genre_source == "essentia" && !cfg.essentia_url.trim().is_empty() {
                let n = crate::stats::host_stats::write_essentia_genres(cfg, &root, &plan, &files);
                if n > 0 {
                    actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": format!("essentia genres: {n} track(s) tagged"),
                    }));
                }
            }
            if cfg.scan_after_tag_write {
                if let Err(e) = crate::wasm::trigger_navidrome_scan(cfg) {
                    crate::wasm::log_warn(&format!("scan trigger failed: {e}"));
                }
            }
            if cfg.notify_audiomuse_after_run && !cfg.audiomuse_url.trim().is_empty() {
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

/// Auto-tag a group's tracks from the MusicBrainz release tracklist, filling
/// only genuinely-missing fields (title/artist/recording MBID/release MBID).
/// Apply mode writes (atomically); dry-run only counts what would change.
/// Returns Some(count) when any track was/would be tagged.
fn autotag_album(
    cfg: &Config,
    root: &std::path::Path,
    files: &[(String, crate::tags::TrackTags)],
    rel: &crate::musicbrainz::MbRelease,
    info: &crate::organizer::AlbumInfo,
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
                let abs = root.join(relpath);
                match crate::tags::fill_missing_from_mb(
                    &abs,
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
        timeout_ms: 30_000,
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
    for p in &plan.unverified {
        s.push_str(&format!("    - {p}  --  unverified (no MBID/ISRC); routed to Singles folder.\n"));
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
    root: &Path,
    cfg: &Config,
    plan: &crate::organizer::GroupPlan,
    files: &[(String, TrackTags)],
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
        files.first().and_then(|(rel, _)| {
            let abs = root.join(rel);
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
                    "genres": false,
                    "moods": false,
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
    let nfo_album = crate::nfo::NfoAlbum {
        title: info.album.clone(),
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
    let path = root.join(&plan.target_dir).join("album.nfo");
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Err(e) = crate::tags::atomic_write(&path, crate::nfo::serialize_album(&nfo_album).as_bytes()) {
        crate::wasm::log_warn(&format!("write album.nfo: {e}"));
    }
    // Also write artist.nfo into the artist folder (parent of the album dir)
    // with the artist name + genres + similar artists. Kodi reads it there.
    if let Some(artist_dir) = Path::new(&plan.target_dir).parent() {
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
            // Read existing artist.nfo if present to preserve other fields.
            let a_path = root.join(artist_dir).join("artist.nfo");
            let existing_nfo = if let Ok(xml) = std::fs::read_to_string(&a_path) {
                crate::nfo::parse_artist_nfo(&xml)
            } else {
                None
            };
            let nfo_artist = crate::nfo::NfoArtist {
                name: info.album_artist.clone(),
                genres: genre.clone(),
                similar_artists,
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
    root: &Path,
    plan: &crate::organizer::GroupPlan,
    files: &[(String, TrackTags)],
) -> usize {
    use std::collections::HashMap;
    let by_src: HashMap<&str, &TrackTags> = files.iter().map(|(r, t)| (r.as_str(), t)).collect();
    let mut written = 0usize;
    for m in &plan.moves {
        let Some(t) = by_src.get(m.from.as_str()) else { continue };
        if !crate::wasm::should_write_tags(cfg, &t.album_artist) {
            continue;
        }
        let Some(ac) = crate::audiomuse::fetch(cfg, &t.artist, &t.title) else {
            continue;
        };
        let final_path = root.join(&m.to);
        match crate::audiomuse::write_tags(&final_path, &ac, cfg.overwrite_existing_tags) {
            Ok(()) => written += 1,
            Err(e) => crate::wasm::log_warn(&format!("acoustic tags for {}: {e}", m.from)),
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
    root: &Path,
    plan: &crate::organizer::GroupPlan,
    files: &[(String, TrackTags)],
    format: &str,
    lyrics_source: &str,
    cfg: &crate::config::Config,
) -> usize {
    use std::collections::HashMap;
    let by_src: HashMap<&str, &TrackTags> = files.iter().map(|(r, t)| (r.as_str(), t)).collect();
    let mut written = 0usize;
    for m in &plan.moves {
        let Some(t) = by_src.get(m.from.as_str()) else { continue };
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
        let final_path = root.join(&m.to);
        match crate::lyrics::write_sidecar(&final_path, &lyr, format) {
            Ok(()) => written += 1,
            Err(e) => crate::wasm::log_warn(&format!("lyrics for {}: {e}", m.from)),
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
/// Returns (genres, source_name) on success.
fn fetch_genre_with_fallback(
    cfg: &crate::config::Config,
    mbid: Option<&str>,
    artist: &str,
    album: &str,
    nfo_genres: &[String],
) -> Option<(Vec<String>, String)> {
    let sources = match cfg.genre_source.as_str() {
        "musicbrainz" => vec!["musicbrainz", "discogs", "theaudiodb", "essentia", "nfo"],
        "discogs" => vec!["discogs", "musicbrainz", "theaudiodb", "essentia", "nfo"],
        "theaudiodb" => vec!["theaudiodb", "musicbrainz", "discogs", "essentia", "nfo"],
        "essentia" => vec!["essentia", "musicbrainz", "discogs", "theaudiodb", "nfo"],
        "nfo" => vec!["nfo", "musicbrainz", "discogs", "theaudiodb", "essentia"],
        _ => vec!["musicbrainz", "discogs", "theaudiodb", "essentia", "nfo"],
    };
    for source in sources {
        match source {
            "musicbrainz" => {
                if let Some(m) = mbid {
                    if let Some(genres) = crate::musicbrainz::fetch_genres(m, &cfg.musicbrainz_token) {
                        return Some((genres, "musicbrainz".into()));
                    }
                }
            }
            "discogs" => {
                if !cfg.discogs_token.is_empty() {
                    if let Some(genres) = crate::discogs::host_discogs::fetch_genres(cfg, artist, album) {
                        return Some((genres, "discogs".into()));
                    }
                }
            }
            "theaudiodb" => {
                if !cfg.theaudiodb_key.is_empty() {
                    if let Some(genres) = crate::theaudiodb::host_theaudiodb::fetch_genres(cfg, artist, album) {
                        return Some((genres, "theaudiodb".into()));
                    }
                }
            }
            "essentia" => {
                // Essentia genres are written by write_essentia_genres() separately.
                // In the fallback chain, skip Essentia and try other sources.
            }
            "nfo" => {
                if !nfo_genres.is_empty() {
                    return Some((nfo_genres.to_vec(), "nfo".into()));
                }
            }
            _ => {}
        }
    }
    None
}




