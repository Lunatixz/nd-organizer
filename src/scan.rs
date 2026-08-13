// Full-library scan: a chunked, resumable walk that builds a metadata index,
// then groups files into albums by their TAGS (not folders) and plans/applies
// the result. Only available on the wasm target (uses host services).

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
fn index_file(library_id: i32, rel: &str, abs: &Path) -> Result<(), String> {
    let mtime = file_mtime(abs);
    let key = file_key(library_id, rel);
    if let Ok(Some(v)) = crate::store::kv().get(&key) {
        if let Ok(val) = serde_json::from_slice::<Value>(&v) {
            if val.get("mtime").and_then(|m| m.as_i64()) == Some(mtime) {
                return Ok(());
            }
        }
    }
    let tags = crate::tags::read_tags(abs);
    let entry = match tags {
        Some(t) => json!({ "rel": rel, "tags": t, "mtime": mtime }),
        None => json!({ "rel": rel, "tags": null, "mtime": mtime }),
    };
    crate::store::kv().set(&key, entry.to_string().into_bytes()).map_err(|e| e.to_string())
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
    let mut hit_limit = false;
    let mut last_rel: String = String::new();

    while let Some(dir_rel) = stack.pop() {
        if crate::organizer::is_excluded(&dir_rel, &cfg.exclude_paths) {
            continue;
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
                index_file(library_id, &rel, &entry.path())?;
                processed += 1;
                if processed >= files_per_task {
                    hit_limit = true;
                    break;
                }
            }
        }
        for sub in subdirs.into_iter().rev() {
            stack.push(sub);
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

/// AcoustID circuit breaker: when the sidecar drops mid-run, pause the batch
/// (retry window), then stop for a cooldown, then resume WITHOUT fingerprinting
/// rather than blocking the run on Nx30s timeouts. State is just a failure
/// timestamp in KVStore; the stage is derived from how long ago it failed
/// (no background timers needed). A live probe (throttled) recovers the circuit.
const CIRCUIT_KEY: &str = "acoustid";

fn circuit_clear() {
    crate::net::circuit_clear(CIRCUIT_KEY);
}

fn circuit_mark_failed() {
    crate::net::circuit_mark_failed(CIRCUIT_KEY);
}

fn circuit_stage() -> Option<crate::state::AcoustidStage> {
    crate::net::circuit_stage(CIRCUIT_KEY)
}

/// Live `/health` probe (not the 1-hour-cached probe_ok), throttled to one per
/// minute so the pause loop can detect recovery on its own cadence. Clears the
/// circuit when the sidecar answers.
fn circuit_recovered(cfg: &Config) -> bool {
    if !crate::net::throttle("acoustid.circuit", 60_000) {
        return false;
    }
    let base = cfg.acoustid_url.trim();
    if base.is_empty() {
        return false;
    }
    let url = format!("{}/health", base.trim_end_matches('/'));
    let req = host::http::HTTPRequest {
        method: "GET".into(),
        url,
        headers: std::collections::HashMap::new(),
        no_follow_redirects: false,
        body: vec![],
        timeout_ms: 5_000,
    };
    let up = matches!(host::http::send(req), Ok(Some(resp)) if resp.status_code == 200);
    if up {
        circuit_clear();
    }
    up
}

/// Ask the AcoustID sidecar (Docker) to fingerprint a file and identify it.
/// Returns the matched album (release group) MBID and recording MBID, if any.
/// Cached per path for 7 days.
pub fn identify_file(cfg: &Config, abs_path: &str) -> Option<(String, Option<String>)> {
    if cfg.acoustid_url.trim().is_empty() || cfg.acoustid_api_key.trim().is_empty() {
        return None;
    }
    // Circuit open (retry/cooldown/degraded): fail fast instead of Nx30s timeouts.
    if circuit_stage().is_some() {
        return None;
    }
    let cache_key = format!("ident:{:016x}", crate::state::fnv1a64(abs_path));
    if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
        if let Ok(val) = serde_json::from_slice::<Value>(&v) {
            let album = val
                .get("album")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if !album.is_empty() {
                let rec = val
                    .get("recording")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                return Some((album, if rec.is_empty() { None } else { Some(rec) }));
            }
        }
    }
    let base = cfg.acoustid_url.trim_end_matches('/');
    let body =
        serde_json::json!({ "path": abs_path, "acoustidApiKey": cfg.acoustid_api_key }).to_string();
    let req = host::http::HTTPRequest {
        method: "POST".into(),
        url: format!("{base}/lookup"),
        headers: std::collections::HashMap::new(),
        no_follow_redirects: false,
        body: body.into_bytes(),
        timeout_ms: 30_000,
    };
    let result: Option<(String, Option<String>)> = match host::http::send(req) {
        Ok(Some(resp)) if resp.status_code == 200 => {
            let body = String::from_utf8_lossy(&resp.body).into_owned();
            let Ok(v) = serde_json::from_str::<Value>(&body) else {
                return None;
            };
            if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
                return None;
            }
            let matches = v
                .get("matches")
                .and_then(|m| m.as_array())
                .cloned()
                .unwrap_or_default();
            for m in matches {
                let rec_id = m
                    .get("recordingId")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let rgs = m
                    .get("releaseGroups")
                    .and_then(|r| r.as_array())
                    .cloned()
                    .unwrap_or_default();
                if let Some(rg) = rgs.first() {
                    if let Some(id) = rg.get("id").and_then(|x| x.as_str()) {
                        return Some((id.to_string(), Some(rec_id)));
                    }
                }
            }
            None
        }
        // Any transport-level failure (connection refused/timeout/non-200) opens
        // the circuit so the run pauses instead of hammering a dead sidecar.
        _ => {
            circuit_mark_failed();
            None
        }
    };
    if result.is_some() {
        circuit_clear();
    }
    let cached = match &result {
        Some((a, r)) => serde_json::json!({ "album": a, "recording": r.as_deref().unwrap_or("") }),
        None => serde_json::Value::Null,
    };
    let _ = crate::store::kv().set_with_ttl(&cache_key, cached.to_string().into_bytes(), 7 * 24 * 3600);
    result
}

/// After the scan completes: load the index, group files into albums by their
/// tags, and enqueue plan tasks for each group (batched). When identity
/// verification is on, files without any MBID/ISRC are fingerprinted via the
/// AcoustID sidecar; files that still can't be identified are left in place.
/// Returns `(plan_tasks_enqueued, files_grouped)`.
pub fn group_step(cfg: &Config, library_id: i32) -> Result<(usize, usize), String> {
    let real_root = library_real_path(library_id)?;

    // AcoustID circuit breaker: pause the batch while the sidecar is down, wait
    // for it to come back, and only degrade (run without fingerprinting) after
    // the retry + cooldown windows both expire. The next scheduler pass re-runs
    // scan -> group, so pausing here naturally re-checks on its own cadence.
    if let Some(stage) = circuit_stage() {
        use crate::state::AcoustidStage;
        match stage {
            AcoustidStage::Degraded => {
                crate::wasm::log_info(
                    "AcoustID offline past cooldown - resuming run WITHOUT fingerprinting; unverified files left in place",
                );
            }
            AcoustidStage::Retry | AcoustidStage::Cooldown => {
                if circuit_recovered(cfg) {
                    crate::wasm::log_info("AcoustID is back online - resuming run");
                } else {
                    let msg = match stage {
                        AcoustidStage::Retry => {
                            "AcoustID is offline - pausing run; waiting to see if the sidecar comes back online"
                        }
                        _ => {
                            "AcoustID still offline - run stopped; waiting out the cooldown window"
                        }
                    };
                    crate::wasm::log_info(msg);
                    crate::wasm::post_webhook(
                        cfg,
                        &format!("nd-organizer: run paused - AcoustID offline. Resuming when the sidecar returns or after cooldown."),
                    );
                    return Ok((0, 0));
                }
            }
        }
    }

    let prefix = format!("scan.filev2.{library_id}:");
    let keys = crate::store::kv().list(&prefix).map_err(|e| e.to_string())?;
    let values = crate::store::kv().get_many(keys).map_err(|e| e.to_string())?;

    let mut entries: Vec<(String, TrackTags)> = Vec::new();
    for (_k, v) in values {
        let Ok(val) = serde_json::from_slice::<Value>(&v) else {
            continue;
        };
        let Some(tags) = val.get("tags") else {
            continue;
        };
        if tags.is_null() {
            continue;
        }
        if let Ok(t) = serde_json::from_value::<TrackTags>(tags.clone()) {
            let rel = val
                .get("rel")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            if !rel.is_empty() {
                entries.push((rel, t));
            }
        }
    }

    // Verification: files without a reliable ID are either fingerprinted via
    // AcoustID (giving an album MBID to group by) or left unverified.
    let total_files = entries.len();
    let mut verified: Vec<(String, TrackTags)> = Vec::new();
    let mut unverified = 0usize;
    if cfg.verify_identity {
        for (rel, t) in entries {
            // AcoustID dropped mid-batch: pause now; verified files still get
            // grouped, the rest resume next pass. (Degraded mode already
            // fail-fasts in identify_file, so no pause there.)
            if matches!(
                circuit_stage(),
                Some(crate::state::AcoustidStage::Retry | crate::state::AcoustidStage::Cooldown)
            ) {
                crate::wasm::log_info("AcoustID went offline mid-batch - pausing run; resuming on a later pass");
                crate::wasm::post_webhook(cfg, "nd-organizer: AcoustID went offline mid-batch - run paused");
                return Ok((0, 0));
            }
            if crate::identity::score(&t, None) >= cfg.min_confidence {
                verified.push((rel, t));
            } else {
                let abs = format!("{real_root}/{rel}");
                match identify_file(cfg, &abs) {
                    Some((album_mbid, rec_mbid)) => {
                        // Persist the resolved identity in the file's own tags
                        // (apply mode only) so future runs skip AcoustID for it.
                        if cfg.mode == Mode::Apply && !album_mbid.is_empty() {
                            if crate::wasm::should_write_tags(cfg, &t.album_artist) {
                                if cfg.backup_before_write {
                                    let _ = crate::state::backup_tag_state(
                                        &crate::wasm::current_run_id(library_id).unwrap_or_default(),
                                        &abs,
                                        &t,
                                    );
                                }
                                if let Err(e) = crate::tags::write_mbids(
                                    Path::new(&abs),
                                    &album_mbid,
                                    rec_mbid.as_deref(),
                                    cfg.overwrite_existing_tags,
                                ) {
                                    crate::wasm::log_warn(&format!("write MBID tags {rel}: {e}"));
                                }
                            }
                        }
                        let mut t2 = t;
                        t2.mbid_album = album_mbid;
                        verified.push((rel, t2));
                    }
                    None => {
                        if cfg.skip_unverified {
                            unverified += 1;
                        } else {
                            // skipUnverified off: still organize by tag/folder
                            // heuristics (pairing is less certain, but nothing
                            // is left stranded).
                            verified.push((rel, t));
                        }
                    }
                }
            }
        }
    } else {
        verified = entries;
    }

    if unverified > 0 {
        crate::wasm::log_info(&format!(
            "library {library_id}: {unverified} files could not be verified (no MBID/ISRC/AcoustID); left in place"
        ));
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
    // After the plan/apply work runs, sweep for folders left with no audio
    // (images/nfo/lyrics/misc only) - gated by cleanupNoAudioFolders.
    if cfg.cleanup_no_audio_folders {
        crate::wasm::enqueue_cleanup_task(library_id)?;
    }
    Ok((enqueued, total_files))
}

/// Delete folders under the library root whose entire subtree contains NO audio
/// files (only images/nfo/lyrics/misc remain). Handles both empty folders and
/// folders left behind after moves. Apply mode only - dry-run reports what would
/// be deleted. Never deletes the library root or anything inside an excluded
/// path. Returns how many folders were removed (or would be, in dry-run).
pub fn cleanup_step(cfg: &Config, library_id: i32) -> Result<usize, String> {
    let root = lib_root(library_id)?;
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

/// Plan (and in apply mode, apply) a batch of album groups.
pub fn plan_step(
    cfg: &Config,
    library_id: i32,
    groups: &[Vec<String>],
    batch_index: i32,
    batch_total: i32,
) -> Result<(), String> {
    let eff = crate::wasm::effective_config(cfg);
    let cfg = &eff;
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
        // Optional: MusicBrainz release type drives classification (classifyFromMB
        // + primarySource = musicbrainz). Looked up per album, cached 7 days.
        let mb_type = if cfg.classify_from_mb
            && cfg.primary_source == crate::config::PrimarySource::MusicBrainz
        {
            crate::musicbrainz::lookup(&info.album_artist, &info.album, &cfg.musicbrainz_token)
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
                .unwrap_or_default()
        } else {
            String::new()
        };
        // Optional: force-search incomplete albums that are monitored in Lidarr.
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
        // Dry-run visibility: show the current star rating + playcount that each
        // tracked file in this album would carry (from the plugin tally).
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
            // Live action ticker: record each concrete action taken for this album.
            for m in &plan.moves {
                actions.push(serde_json::json!({
                    "ts": crate::state::now_ts(),
                    "text": format!("moved {} -> {}", m.from, m.to),
                }));
            }
            // Record each move so the run can be rolled back.
            let run_id = crate::wasm::current_run_id(library_id)?;
            // Back up the album's current album.nfo (if any) BEFORE we rewrite it,
            // so rollback can restore the original content.
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
                // Carry the star tally to the file's new path so its rating and
                // playcount survive the rename/move.
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
            // Download + embed/save album artwork (Cover Art Archive).
            if cfg.embed_artwork || cfg.write_cover_jpg {
                if let Some(summary) = apply_artwork(cfg, &root, &plan, &files) {
                    actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": summary,
                    }));
                }
            }
            // Fetch + write lyrics sidecars (.lrc / .txt) next to the moved files.
            if cfg.download_lyrics {
                let n = download_lyrics_for(&root, &plan, &files, cfg.lyrics_format.as_str());
                if n > 0 {
                    actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": format!("lyrics: fetched {n} sidecar(s)"),
                    }));
                }
            }
            // Write acoustic tags (BPM/key/mood/energy) from AudioMuse-AI.
            if cfg.write_acoustic_tags && !cfg.audiomuse_url.trim().is_empty() {
                let n = write_acoustic_tags_for(cfg, &root, &plan, &files);
                if n > 0 {
                    actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": format!("acoustic tags: BPM/key/mood for {n} track(s)"),
                    }));
                }
            }
            // Scan as we go so the player never points at moved files or stale
            // tags (scanAfterAlbum = after moves; scanAfterTagWrite = tag/NFO
            // writes too).
            if cfg.scan_after_album || cfg.scan_after_tag_write {
                if let Err(e) = crate::wasm::trigger_navidrome_scan(cfg) {
                    crate::wasm::log_warn(&format!("scan trigger failed: {e}"));
                }
            }
            // Ask AudioMuse-AI to re-sync after file moves so its analysis stays
            // valid for the new paths (notifyAudiomuseAfterRun).
            if cfg.notify_audiomuse_after_run && !cfg.audiomuse_url.trim().is_empty() {
                match crate::audiomuse::re_sync(cfg) {
                    Ok(()) => actions.push(serde_json::json!({
                        "ts": crate::state::now_ts(),
                        "text": "audiomuse: requested re-sync".to_string(),
                    })),
                    Err(e) => crate::wasm::log_warn(&format!("AudioMuse-AI re-sync: {e}")),
                }
            }
            // Keep Lidarr's DB in sync after we move files for this album
            // (only in metadataPlusRescan mode, and once per artist per 5 min).
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
            }
        } else if cfg.mode != Mode::Apply {
            // Dry-run: record the would-be moves so the action ticker is honest
            // in preview mode too.
            for m in &plan.moves {
                actions.push(serde_json::json!({
                    "ts": crate::state::now_ts(),
                    "text": format!("would move {} -> {}", m.from, m.to),
                }));
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
    // Dry run: the report is a full simulation of the work a real run would do,
    // clearly labelled so the user can trust the plan before applying it.
    if cfg.mode != Mode::Apply {
        report_text = format!(
            "[DRY RUN] batch {}/{} - simulated, nothing changed.\n\
             Switch mode to 'apply' to execute exactly these actions.\n{}\n",
            batch_index + 1,
            batch_total.max(1),
            report_text
        );
    }
    // Always surface the run id so the user knows what to roll back.
    let run_id = crate::wasm::current_run_id(library_id).unwrap_or_default();
    report_text.push_str(&format!(
        "\n[rollback] Run ID: {run_id}\nTo undo everything in this run, set 'rollbackRunId' = {run_id} in the plugin settings, then run a pass.\n"
    ));
    crate::wasm::save_report(&report_text, cfg.backup_retention_days as i64);
    crate::wasm::log_info(&report_text);
    // Post the report as a structured envelope so the dashboard can render the
    // moves/albums and clearly label each entry DRY RUN vs APPLY.
    let report_envelope = serde_json::json!({
        "ts": crate::state::now_ts(),
        "mode": crate::wasm::mode_label(cfg),
        "kind": "report",
        "dryRun": cfg.mode != Mode::Apply,
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
    crate::wasm::log_info(&format!(
        "STATUS: library={} mode={} batch={}/{} albumsToMove={} fileMoves={} duplicates={}",
        library_id,
        crate::wasm::mode_label(cfg),
        batch_index + 1,
        batch_total.max(1),
        total_to_move,
        total_moves,
        total_dupes
    ));
    // Also push the status JSON so the webhook dashboard's Status card updates
    // on every batch, not just on deferral events.
    let status_json = serde_json::json!({
        "ts": crate::state::now_ts(),
        "mode": crate::wasm::mode_label(cfg),
        "inProgress": false,
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
        s.push_str(&format!("    - {p}  --  unverified (no MBID/ISRC); not moved. Set up AcoustID to identify it.\n"));
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
    let nfo_album = crate::nfo::NfoAlbum {
        title: info.album.clone(),
        album_artists: if info.album_artist.is_empty() {
            vec![]
        } else {
            vec![info.album_artist.clone()]
        },
        year: info.year,
        genres: genre.clone(),
        ..Default::default()
    };
    let path = root.join(&plan.target_dir).join("album.nfo");
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Err(e) = std::fs::write(&path, crate::nfo::serialize_album(&nfo_album)) {
        crate::wasm::log_warn(&format!("write album.nfo: {e}"));
    }
    let _ = cfg;
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
) -> usize {
    use std::collections::HashMap;
    let by_src: HashMap<&str, &TrackTags> = files.iter().map(|(r, t)| (r.as_str(), t)).collect();
    let mut written = 0usize;
    for m in &plan.moves {
        let Some(t) = by_src.get(m.from.as_str()) else { continue };
        let Some(lyr) = crate::lyrics::fetch(&t.artist, &t.title, &t.album, 0) else {
            continue;
        };
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

/// Download + embed/save album artwork for a group (Cover Art Archive). Uses the
/// album's release MBID; honors the enabled kinds, the overwrite rule, and the
/// priority list (embedded / folder art are preferred when listed first).
fn apply_artwork(
    cfg: &Config,
    root: &Path,
    plan: &crate::organizer::GroupPlan,
    files: &[(String, TrackTags)],
) -> Option<String> {
    use crate::artwork::ArtKind;
    let mbid = files
        .iter()
        .find_map(|(_, t)| (!t.mbid_album.trim().is_empty()).then(|| t.mbid_album.clone()));
    let Some(mbid) = mbid else {
        crate::wasm::log_info(&format!(
            "artwork: no release MBID for {} - skipping",
            plan.target_dir
        ));
        return None;
    };
    // artworkPriority picks the external source. Only 'coverartarchive' is
    // implemented as a download source; 'embedded'/'itunes' keep whatever art
    // the files/folder already have, so no external fetch happens.
    if cfg.artwork_priority != "coverartarchive" {
        crate::wasm::log_info(&format!(
            "artwork: priority '{}' keeps existing art (only 'coverartarchive' downloads new)",
            cfg.artwork_priority
        ));
        return None;
    }
    let kinds = {
        let mut k = Vec::new();
        if cfg.artwork_front {
            k.push(ArtKind::Front);
        }
        if cfg.artwork_back {
            k.push(ArtKind::Back);
        }
        if cfg.artwork_cd {
            k.push(ArtKind::Cd);
        }
        if cfg.artwork_booklet {
            k.push(ArtKind::Booklet);
        }
        k
    };
    let dir = root.join(&plan.target_dir);
    let mut embedded = 0usize;
    let mut sidecar = false;
    for kind in kinds {
        let Some(bytes) = crate::artwork::fetch(&mbid, kind) else {
            continue;
        };
        if cfg.embed_artwork {
            // overwriteArt=false: keep existing embedded art.
            let first = files.first().map(|(r, _)| root.join(r)).unwrap_or_default();
            if cfg.overwrite_art || !crate::artwork::has_embedded(&first) {
                for (rel, _) in files {
                    let path = root.join(rel);
                    if crate::artwork::embed(&path, bytes.clone(), kind).is_ok() {
                        embedded += 1;
                    }
                }
            }
        }
        if kind == ArtKind::Front && cfg.write_cover_jpg {
            if cfg.overwrite_art || !dir.join("cover.jpg").exists() {
                if crate::artwork::write_sidecar(&dir, bytes.clone()).is_ok() {
                    sidecar = true;
                }
            }
        }
    }
    if embedded > 0 || sidecar {
        crate::wasm::log_info(&format!(
            "artwork: embedded {embedded} image(s){}{} for {}",
            if sidecar { " + cover.jpg" } else { "" },
            if cfg.embed_artwork && !cfg.artwork_front { "" } else { "" },
            plan.target_dir
        ));
        Some(format!(
            "artwork: embedded {embedded} image(s){}",
            if sidecar { " + cover.jpg" } else { "" }
        ))
    } else {
        None
    }
}


