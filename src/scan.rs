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

/// Scan the next chunk of the library. Returns true if more remains.
pub fn scan_step(cfg: &Config, library_id: i32) -> Result<bool, String> {
    let root = lib_root(library_id)?;
    let key = stack_key(library_id);
    let mut stack = load_stack(&key);
    let files_per_task = cfg.files_per_scan_task.max(1);
    let mut processed = 0usize;
    let mut hit_limit = false;

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

    if hit_limit {
        crate::store::kv().set(&key, serde_json::to_vec(&stack).unwrap_or_default())
            .map_err(|e| e.to_string())?;
        crate::wasm::enqueue_scan_task(library_id)?;
        post_scan_status(cfg, library_id, processed);
        Ok(true)
    } else {
        let _ = crate::store::kv().delete(&key);
        let _ = crate::store::kv().set(&format!("scan.donev2.{library_id}"), b"1".to_vec());
        crate::wasm::enqueue_group_task(library_id)?;
        post_scan_status(cfg, library_id, processed);
        Ok(false)
    }
}

/// Push a scan-progress status to the webhook dashboard after every chunk so
/// the user sees activity during the (slow) library scan, not just at the end.
fn post_scan_status(cfg: &Config, library_id: i32, chunk: usize) {
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

/// Ask the AcoustID sidecar (Docker) to fingerprint a file and identify it.
/// Returns the matched album (release group) MBID and recording MBID, if any.
/// Cached per path for 7 days.
pub fn identify_file(cfg: &Config, abs_path: &str) -> Option<(String, Option<String>)> {
    if cfg.acoustid_url.trim().is_empty() || cfg.acoustid_api_key.trim().is_empty() {
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
        _ => None,
    };
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
pub fn group_step(cfg: &Config, library_id: i32) -> Result<usize, String> {
    let real_root = library_real_path(library_id)?;
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
    let mut verified: Vec<(String, TrackTags)> = Vec::new();
    let mut unverified = 0usize;
    if cfg.verify_identity {
        for (rel, t) in entries {
            if crate::identity::confidence(&t) == crate::identity::Confidence::Verified {
                verified.push((rel, t));
            } else {
                let abs = format!("{real_root}/{rel}");
                match identify_file(cfg, &abs) {
                    Some((album_mbid, _)) => {
                        let mut t2 = t;
                        t2.mbid_album = album_mbid;
                        verified.push((rel, t2));
                    }
                    None => unverified += 1,
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
    let enqueued = crate::wasm::enqueue_plan_tasks(cfg, library_id, groups)?;
    Ok(enqueued)
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
    let mut total_moves = 0usize;
    let mut total_dupes = 0usize;
    let mut total_to_move = 0usize;

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
        let plan = crate::organizer::build_group_plan(&root, cfg, &files, &folder_hint);
        total_moves += plan.moves.len();
        total_dupes += plan.duplicates.len();
        total_to_move += usize::from(!plan.moves.is_empty());
        report_parts.push(group_report(&plan));

        if cfg.mode == Mode::Apply && !plan.moves.is_empty() {
            crate::organizer::apply_group_plan(&root, &plan, cfg.prune_empty_dirs)?;
            // Record each move so the run can be rolled back.
            let run_id = crate::wasm::current_run_id(library_id)?;
            for m in &plan.moves {
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
                    nfo_written: None,
                    nfo_backup: None,
                };
                if let Err(e) = crate::state::host_state::record_apply(&mut rec) {
                    crate::wasm::log_warn(&format!("record apply {}: {e}", m.from));
                }
            }
            if cfg.write_nfo {
                write_group_nfo(&root, cfg, &plan, &files);
            }
            // Scan as we go so the player never points at moved files.
            if cfg.scan_after_album {
                if let Err(e) = crate::wasm::trigger_navidrome_scan(cfg) {
                    crate::wasm::log_warn(&format!("scan trigger failed: {e}"));
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
                            Ok(()) => crate::wasm::log_info(&format!(
                                "Lidarr: RefreshArtist submitted for {}",
                                lidar.artist
                            )),
                            Err(e) => crate::wasm::log_warn(&format!("Lidarr RefreshArtist failed: {e}")),
                        }
                    }
                }
            }
        }
    }

    let report_text = if report_parts.is_empty() {
        format!(
            "No albums in batch {}/{}\n",
            batch_index + 1,
            batch_total.max(1)
        )
    } else {
        report_parts.join("\n")
    };
    crate::wasm::save_report(&report_text, cfg.backup_retention_days as i64);
    crate::wasm::log_info(&report_text);
    crate::wasm::post_webhook(cfg, &report_text);
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
        "warnings": [],
        "integrations": crate::wasm::integration_health(cfg),
        "tasks": crate::wasm::task_log(),
    })
    .to_string();
    crate::wasm::post_webhook(cfg, &status_json);
    Ok(())
}

/// A plain-language report block for one album group.
fn group_report(plan: &crate::organizer::GroupPlan) -> String {
    let mut s = String::new();
    let kind = match plan.bucket {
        crate::organizer::Bucket::Soundtrack => "Soundtrack",
        crate::organizer::Bucket::Various => "Various artists (compilation)",
        crate::organizer::Bucket::Singles => "Single / incomplete",
        crate::organizer::Bucket::Normal => "Normal album",
    };
    s.push_str(&format!("--- Album ({kind}) ---\n"));
    s.push_str(&format!("  Target folder: /{}\n", plan.target_dir));
    if !plan.moves.is_empty() {
        s.push_str(&format!("  Files to move ({}):\n", plan.moves.len()));
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
                "    - {loser}  is a duplicate of  {winner}  -> moved to /{target}\n",
                loser = dup.loser,
                winner = dup.winner,
                target = dup.target
            ));
        }
    }
    if !plan.fillers.is_empty() {
        s.push_str(&format!(
            "  Filler tracks flagged (dropped by the filter proxy, files kept) ({}):\n",
            plan.fillers.len()
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


