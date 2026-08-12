// nd-organizer — Navidrome music organizer plugin.
//
// Pure logic (config parsing, templates, classification, plan building) lives
// in modules that compile on the host so `cargo test` can exercise them. The
// Extism/Navidrome glue (capability exports, host-service calls) is gated to
// the wasm32 target, where the plugin actually runs.

mod config;
mod nfo;
mod organizer;
mod report;
mod state;
#[cfg(target_arch = "wasm32")]
mod store;
mod tags;
mod template;

#[cfg(target_arch = "wasm32")]
mod wasm {
    // Capabilities:
    //   - Lifecycle:   create the task queue, schedule a startup run.
    //   - Scheduler:   on a scheduled tick, run a pass (dry-run report always,
    //                  apply tasks enqueued only in apply mode).
    //   - TaskWorker:  apply one album's rename plan (only called in apply mode).

    use std::collections::HashMap;
    use std::path::Path;

    use nd_pdk::host;
    use nd_pdk::lifecycle::InitProvider;
    use nd_pdk::register_lifecycle_init;
    use nd_pdk::register_scheduler_callback;
    use nd_pdk::register_taskworker_task_execute;
    use nd_pdk::scheduler::{CallbackProvider, SchedulerCallbackRequest};
    use nd_pdk::taskworker::{TaskExecuteProvider, TaskExecuteRequest};
    use serde::{Deserialize, Serialize};

    use super::config::{Config, Mode};
    use super::nfo::{self, NfoAlbum, NfoArtist};
    use super::organizer::{
        album_info_with_nfo, apply_plan, build_plan, discover_albums_skip, Bucket,
    };
    use super::report;
    use super::state::{self, ApplyRecord, FileRename};
    use super::store;
    use super::template;

    const QUEUE: &str = "organize";
    const RUN_SCHEDULE_ID: &str = "nd-organizer-startup-run";

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct TaskPayload {
        /// "pass" = run a batch of albums; "album" = apply one album.
        kind: String,
        library_id: i32,
        dir: String,
        run_id: String,
        /// Album dirs to plan in this pass task (small batches).
        dirs: Vec<String>,
        batch_index: i32,
        batch_total: i32,
    }

    fn enqueue_payload(p: &TaskPayload) -> Result<(), String> {
        let bytes = serde_json::to_vec(p).map_err(|e| e.to_string())?;
        host::task::enqueue(QUEUE, bytes).map(|_| ()).map_err(|e| e.to_string())
    }

    fn enqueue(kind: &str, library_id: i32, dir: &str, run_id: &str) -> Result<(), String> {
        enqueue_payload(&TaskPayload {
            kind: kind.into(),
            library_id,
            dir: dir.into(),
            run_id: run_id.into(),
            dirs: vec![],
            batch_index: -1,
            batch_total: -1,
        })
    }

    #[derive(Default)]
    struct NdOrganizer;

    impl InitProvider for NdOrganizer {
        fn on_init(&self) -> Result<(), nd_pdk::lifecycle::Error> {
            let cfg = Config::load().map_err(|e| nd_pdk::lifecycle::Error::new(e))?;
            log_info(&format!(
                "nd-organizer v{} init: mode={:?} libraries={:?}",
                env!("CARGO_PKG_VERSION"),
                cfg.mode,
                cfg.libraries
            ));
            log_library_inventory();
            // Create the organize queue. Concurrency 1 keeps per-album scans
            // coherent (only one album mid-move when a scan runs).
            let _ = host::task::create_queue(
                QUEUE,
                host::task::QueueConfig {
                    concurrency: 1,
                    max_retries: 1,
                    backoff_ms: 1_000,
                    delay_ms: 0,
                    retention_ms: 3_600_000,
                },
            );
            if cfg.run_on_startup {
                if let Err(e) = host::scheduler::schedule_one_time(15, "startup", RUN_SCHEDULE_ID) {
                    log_warn(&format!("schedule startup run: {e}"));
                }
            }
            Ok(())
        }
    }

    impl CallbackProvider for NdOrganizer {
        fn on_callback(&self, _req: SchedulerCallbackRequest) -> Result<(), nd_pdk::scheduler::Error> {
            let cfg = Config::load().map_err(|e| nd_pdk::scheduler::Error::new(e))?;
            match run_pass(&cfg) {
                Ok(()) => log_info("run pass completed"),
                Err(e) => log_error(&e),
            }
            Ok(())
        }
    }

    impl TaskExecuteProvider for NdOrganizer {
        fn on_task_execute(&self, req: TaskExecuteRequest) -> Result<String, nd_pdk::taskworker::Error> {
            if req.queue_name != QUEUE {
                return Err(nd_pdk::taskworker::Error::new(format!("unknown queue {}", req.queue_name)));
            }
            let payload: TaskPayload = serde_json::from_slice(&req.payload)
                .map_err(|e| nd_pdk::taskworker::Error::new(format!("bad payload: {e}")))?;
            let cfg = Config::load().map_err(|e| nd_pdk::taskworker::Error::new(e))?;
            match payload.kind.as_str() {
                "pass" => run_pass_batch(&cfg, &payload)
                    .map(|st| format!("pass done (batch {}/{})", st.batch_index, st.batch_total))
                    .map_err(|e| nd_pdk::taskworker::Error::new(e)),
                "album" => apply_one_album(&cfg, payload.library_id, &payload.dir, &payload.run_id)
                    .map(|_| format!("ok:{}", payload.dir))
                    .map_err(|e| nd_pdk::taskworker::Error::new(e)),
                other => Err(nd_pdk::taskworker::Error::new(format!("unknown task kind {other}"))),
            }
        }
    }

    /// Libraries to process: every library the plugin has been granted access
    /// to via the Navidrome "Library Access" permission (the authority).
    /// Any legacy `libraries`/`libraryId` config value is ignored.
    fn target_libraries(_cfg: &Config) -> Vec<i32> {
        match host::library::get_all_libraries() {
            Ok(libs) if !libs.is_empty() => libs.into_iter().map(|l| l.id).collect(),
            Ok(_) => {
                log_warn("no accessible libraries; grant Library Access in the plugin settings");
                Vec::new()
            }
            Err(e) => {
                log_warn(&format!("cannot list libraries: {e}"));
                Vec::new()
            }
        }
    }

    /// Per-library summary of a pass batch, used to build the status snapshot.
    #[derive(Default)]
    struct LibraryStatus {
        library_id: i32,
        name: String,
        albums_found: usize,
        albums_to_move: usize,
        file_moves: usize,
        kept: usize,
        skipped: usize,
        batch_index: i32,
        batch_total: i32,
    }

    /// True when nothing is actually playing. Gated by `runOnlyWhenIdle`; uses
    /// the Subsonic getNowPlaying API. Paused/stopped players count as idle -
    /// only entries whose `state` is "playing"/"starting" count as active.
    fn is_idle(cfg: &Config) -> bool {
        if !cfg.run_only_when_idle {
            return true;
        }
        let user = cfg.scan_user.trim();
        if user.is_empty() {
            log_warn("runOnlyWhenIdle is on but scanUser is empty; treating as idle");
            return true;
        }
        match host::subsonicapi::call(&format!("getNowPlaying?u={user}")) {
            Ok(json) => {
                let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
                let entries = v
                    .pointer("/subsonic-response/nowPlaying/entry")
                    .and_then(|e| e.as_array())
                    .cloned()
                    .unwrap_or_default();
                let active = entries
                    .iter()
                    .filter(|e| {
                        matches!(e.get("state").and_then(|s| s.as_str()), Some("playing" | "starting"))
                    })
                    .count();
                if active > 0 {
                    log_info(&format!("playback active ({active} playing); deferring organizer work"));
                }
                active == 0
            }
            Err(e) => {
                log_warn(&format!("idle check failed: {e}; proceeding"));
                true
            }
        }
    }

    /// Build the activity status JSON persisted under `status:latest`.
    fn status_json(
        cfg: &Config,
        in_progress: bool,
        statuses: &[LibraryStatus],
        batch: Option<(i32, i32)>,
    ) -> String {
        let mut libs = Vec::new();
        let mut total_to_move = 0usize;
        let mut total_moves = 0usize;
        for s in statuses {
            total_to_move += s.albums_to_move;
            total_moves += s.file_moves;
            libs.push(serde_json::json!({
                "id": s.library_id,
                "name": s.name,
                "albumsFound": s.albums_found,
                "albumsToMove": s.albums_to_move,
                "fileMoves": s.file_moves,
                "kept": s.kept,
                "skipped": s.skipped,
            }));
        }
        let batch_field = match batch {
            Some((i, n)) => serde_json::json!({"index": i, "total": n}),
            None => serde_json::Value::Null,
        };
        serde_json::json!({
            "ts": state::now_ts(),
            "mode": mode_label(cfg),
            "inProgress": in_progress,
            "batch": batch_field,
            "libraries": libs,
            "totalAlbumsToMove": total_to_move,
            "totalFileMoves": total_moves,
            "warnings": collect_warnings(cfg),
        })
        .to_string()
    }

    /// Probe a service URL (cached 1h) and return a warning when it's bad.
    fn probe_ok(key: &str, url: &str, headers: &HashMap<String, String>) -> Option<String> {
        let cache_key = format!("probe:{}:{}", key, url);
        if let Ok(Some(v)) = host::kvstore::get(&cache_key) {
            let s = String::from_utf8_lossy(&v);
            return if s == "ok" { None } else { Some(s.into_owned()) };
        }
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url: url.to_string(),
            headers: headers.clone(),
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 5_000,
        };
        let result = match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => Ok(()),
            Ok(Some(resp)) => Err(format!("unreachable or wrong credentials (HTTP {})", resp.status_code)),
            Ok(None) => Err("no response".into()),
            Err(e) => Err(format!("connection failed: {e}")),
        };
        let cached = match &result {
            Ok(()) => "ok".to_string(),
            Err(w) => format!("warn:{w}"),
        };
        let _ = host::kvstore::set_with_ttl(&cache_key, cached.into_bytes(), 3600);
        match result {
            Ok(()) => None,
            Err(w) => Some(format!("{key}: {w}")),
        }
    }

    /// Validate config + connectivity and return human-readable warnings shown
    /// in the status JSON so users notice wrong API keys or IP addresses.
    fn collect_warnings(cfg: &Config) -> Vec<String> {
        let mut w = Vec::new();
        if target_libraries(cfg).is_empty() {
            w.push("no libraries are accessible - grant Library Access in the plugin permissions".into());
        }
        if cfg.mode == Mode::Apply {
            for lib in host::library::get_all_libraries().unwrap_or_default() {
                if !lib.mount_point.is_empty() && !library_writable(&lib.mount_point) {
                    w.push(format!(
                        "library {} \"{}\" is READ-ONLY - grant write access via: navidrome plugin edit nd-organizer --write-access",
                        lib.id, lib.name
                    ));
                }
            }
        }
        if cfg.acoustid_mode != crate::config::AcoustIdMode::Disabled && cfg.acoustid_api_key.trim().is_empty() {
            w.push("acoustidApiKey is empty but acoustidMode is enabled".into());
        }
        if cfg.write_playcount && (cfg.lastfm_api_key.trim().is_empty() || cfg.lastfm_user.trim().is_empty()) {
            w.push("writePlaycount requires lastfmApiKey and lastfmUser".into());
        }
        if cfg.genre_from == "lastfm" && cfg.lastfm_api_key.trim().is_empty() {
            w.push("genreFrom is lastfm but lastfmApiKey is empty".into());
        }
        if cfg.use_lidarr_naming_schema && (cfg.lidarr_url.trim().is_empty() || cfg.lidarr_api_key.trim().is_empty()) {
            w.push("useLidarrNamingSchema is on but Lidarr URL/API key are not configured".into());
        }
        if cfg.trigger_scan_after_run && cfg.scan_user.trim().is_empty() {
            w.push("triggerScanAfterRun is on but scanUser is empty (startScan needs a Navidrome admin user)".into());
        }
        if !cfg.lidarr_url.trim().is_empty() {
            let mut h = HashMap::new();
            if !cfg.lidarr_api_key.trim().is_empty() {
                h.insert("X-Api-Key".to_string(), cfg.lidarr_api_key.clone());
            }
            if let Some(war) = probe_ok("lidarr", &format!("{}/api/v1/system/status", cfg.lidarr_url.trim_end_matches('/')), &h) {
                w.push(format!("Lidarr: {war}"));
            }
        }
        if !cfg.audiomuse_url.trim().is_empty() {
            if let Some(war) = probe_ok("audiomuse", cfg.audiomuse_url.trim_end_matches('/'), &HashMap::new()) {
                w.push(format!("AudioMuse-AI: {war}"));
            }
        }
        w
    }

    /// POST a log/report body to the configured webhook (a hosted log).
    fn post_webhook(cfg: &Config, body: &str) {
        let url = cfg.log_webhook_url.trim();
        if url.is_empty() {
            return;
        }
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "text/plain".to_string());
        if !cfg.log_webhook_token.is_empty() {
            headers.insert("X-Token".to_string(), cfg.log_webhook_token.clone());
            headers.insert("Authorization".to_string(), format!("Bearer {}", cfg.log_webhook_token));
        }
        let req = host::http::HTTPRequest {
            method: "POST".into(),
            url: url.to_string(),
            headers,
            no_follow_redirects: false,
            body: body.as_bytes().to_vec(),
            timeout_ms: 10_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) => log_info(&format!("webhook log post: HTTP {}", resp.status_code)),
            Ok(None) => log_warn("webhook log post: no response"),
            Err(e) => log_warn(&format!("webhook log post failed: {e}")),
        }
    }

    /// A scheduled pass. Handles a pending rollback first; otherwise, when the
    /// player is idle, enumerates album dirs (fast, bounded) and enqueues one
    /// small-batch pass task per chunk so resources stay lean.
    fn run_pass(cfg: &Config) -> Result<(), String> {
        if !cfg.rollback_run_id.is_empty() {
            log_info(&format!("rollback requested for run {}", cfg.rollback_run_id));
            return do_rollback(cfg);
        }
        log_library_inventory();
        let target_libs = target_libraries(cfg);
        if target_libs.is_empty() {
            log_error("nothing to organize: no libraries are accessible");
            store::write_status(&status_json(cfg, false, &[], None));
            return Ok(());
        }
        if !is_idle(cfg) {
            log_info("run deferred: playback active (runOnlyWhenIdle); retrying in 120s");
            let status = serde_json::json!({
                "ts": state::now_ts(),
                "mode": mode_label(cfg),
                "inProgress": false,
                "deferredUntilIdle": true,
                "warnings": collect_warnings(cfg),
            })
            .to_string();
            store::write_status(&status);
            post_webhook(cfg, &format!("nd-organizer: run deferred - playback active.\n{status}"));
            let _ = host::scheduler::schedule_one_time(120, "idle-retry", "");
            return Ok(());
        }
        store::write_status(&status_json(cfg, true, &[], None));

        let batch_size = cfg.albums_per_task.max(1);
        let mut enqueued = 0;
        for &library_id in &target_libs {
            let lib = match host::library::get_library(library_id) {
                Ok(Some(lib)) if !lib.mount_point.is_empty() => lib,
                Ok(_) => {
                    log_warn(&format!("library {library_id} has no filesystem mount; skipped"));
                    continue;
                }
                Err(e) => {
                    log_warn(&format!("library {library_id}: {e}"));
                    continue;
                }
            };
            let root = Path::new(&lib.mount_point);
            let limit = if cfg.max_albums_per_run == 0 {
                0usize
            } else {
                cfg.max_albums_per_run
            };
            let albums = discover_albums_skip(
                root,
                cfg.skip_hidden_files,
                &cfg.exclude_paths,
                limit,
                cfg.max_scan_entries,
            );
            let dirs: Vec<String> = albums.into_iter().map(|a| a.dir).collect();
            log_info(&format!("library {library_id}: found {} albums; batching by {batch_size}", dirs.len()));
            let total = dirs.len().div_ceil(batch_size);
            for (i, chunk) in dirs.chunks(batch_size).enumerate() {
                let p = TaskPayload {
                    kind: "pass".into(),
                    library_id,
                    dir: String::new(),
                    run_id: String::new(),
                    dirs: chunk.to_vec(),
                    batch_index: i as i32,
                    batch_total: total as i32,
                };
                match enqueue_payload(&p) {
                    Ok(()) => enqueued += 1,
                    Err(e) => log_warn(&format!("enqueue batch {}/{}: {e}", i + 1, total)),
                }
            }
        }
        log_info(&format!("run pass: enqueued {enqueued} batch tasks"));
        Ok(())
    }

    /// Undo a previously applied run: restore backups, rename files back and
    /// move folders back, then mark the run as rolled back (idempotent).
    fn do_rollback(cfg: &Config) -> Result<(), String> {
        let run_id = cfg.rollback_run_id.trim().to_string();
        if run_id.is_empty() {
            return Ok(());
        }
        if state::host_state::rollback_done(&run_id) {
            log_info(&format!("run {run_id} was already rolled back"));
            return Ok(());
        }
        let mut errors = Vec::new();
        let target_libs = target_libraries(cfg);
        for &library_id in &target_libs {
            let lib = host::library::get_library(library_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("library {library_id} not found"))?;
            if lib.mount_point.is_empty() {
                errors.push(format!("library {library_id} has no filesystem mount"));
                continue;
            }
            let root = Path::new(&lib.mount_point);
            let recs = state::host_state::get_run_applies(&run_id)?;
            let recs: Vec<ApplyRecord> = recs.into_iter().filter(|r| r.library_id == library_id).collect();
            if !recs.is_empty() {
                if let Err(e) = state::host_state::run_rollback(root, &recs) {
                    errors.push(format!("library {library_id}: {e}"));
                } else {
                    log_info(&format!("library {library_id}: rolled back {} applies", recs.len()));
                }
            }
        }
        state::host_state::mark_rollback_done(&run_id)?;
        store::write_status(
            &serde_json::json!({
                "ts": state::now_ts(),
                "mode": mode_label(cfg),
                "inProgress": false,
                "rollbackOfRun": run_id,
                "errors": errors.len(),
            })
            .to_string(),
        );
        if errors.is_empty() {
            log_info(&format!("rollback of run {run_id} complete. Clear the rollbackRunId setting."));
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// Log every library the plugin can see, with its path and access level, so
    /// users can confirm which paths are reachable. The host only returns
    /// libraries the plugin has been granted access to; `path`/`mountPoint` are
    /// only populated when the library filesystem permission is enabled.
    fn log_library_inventory() {
        match host::library::get_all_libraries() {
            Ok(libs) => {
                for lib in libs {
                    let access = if lib.mount_point.is_empty() {
                        "NO ACCESS (grant library access in the plugin settings)"
                    } else if library_writable(&lib.mount_point) {
                        "READ-WRITE"
                    } else {
                        "READ-ONLY (grant write access via: navidrome plugin edit nd-organizer --write-access)"
                    };
                    let path = if lib.path.is_empty() { "?" } else { &lib.path };
                    log_info(&format!(
                        "library {} \"{}\" path={} access={}",
                        lib.id, lib.name, path, access
                    ));
                }
            }
            Err(e) => log_warn(&format!("cannot list libraries (permission missing?): {e}")),
        }
    }

    /// Probe whether a library mount is writable without leaving artifacts.
    fn library_writable(mount: &str) -> bool {
        let probe = Path::new(mount).join(".nd-organizer-write-test");
        match std::fs::write(&probe, b"probe") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    }

    /// When `useLidarrNamingSchema` is on, fetch Lidarr's naming config and
    /// override folder/file schemas with it (cached for 7 days). Falls back to
    /// the plugin schemas when Lidarr is unreachable.
    fn effective_config(cfg: &Config) -> Config {
        if !cfg.use_lidarr_naming_schema {
            return cfg.clone();
        }
        if cfg.lidarr_url.is_empty() || cfg.lidarr_api_key.is_empty() {
            log_warn("useLidarrNamingSchema is on but Lidarr URL/API key are not configured; using plugin schemas");
            return cfg.clone();
        }
        let cache_key = format!("lidarr-naming:{}", cfg.lidarr_url.trim_end_matches('/'));
        if let Ok(Some(json)) = state::host_state::get_cached_meta("lidarr-naming", &cache_key) {
            if let Some(eff) = apply_lidarr_naming(cfg, &json) {
                return eff;
            }
        }
        let url = format!("{}/api/v1/config/naming", cfg.lidarr_url.trim_end_matches('/'));
        let mut headers = HashMap::new();
        headers.insert("X-Api-Key".to_string(), cfg.lidarr_api_key.clone());
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url,
            headers,
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                let body = String::from_utf8_lossy(&resp.body).into_owned();
                let _ = state::host_state::cache_meta("lidarr-naming", &cache_key, &body);
                if let Some(eff) = apply_lidarr_naming(cfg, &body) {
                    return eff;
                }
            }
            Ok(Some(resp)) => log_warn(&format!("Lidarr /config/naming returned HTTP {}", resp.status_code)),
            Ok(None) => log_warn("Lidarr /config/naming returned no response"),
            Err(e) => log_warn(&format!("Lidarr /config/naming request failed: {e}")),
        }
        cfg.clone()
    }

    fn apply_lidarr_naming(cfg: &Config, json: &str) -> Option<Config> {
        let v: serde_json::Value = serde_json::from_str(json).ok()?;
        let artist = v.get("artistFolderFormat").and_then(|x| x.as_str())?;
        let album = v.get("albumFolderFormat").and_then(|x| x.as_str())?;
        let track = v.get("standardTrackFormat").and_then(|x| x.as_str())?;
        let mut eff = cfg.clone();
        eff.folder_schema = format!(
            "{}/{}",
            template::translate_lidarr_format(artist),
            template::translate_lidarr_format(album)
        );
        eff.file_schema = template::translate_lidarr_format(track);
        log_info(&format!(
            "using Lidarr naming schema: folder='{}' file='{}'",
            eff.folder_schema, eff.file_schema
        ));
        Some(eff)
    }

    /// Pass task: plan + report a small batch of albums. Re-checks idle (defer +
    /// retry when playback starts), writes per-batch status, and in apply mode
    /// enqueues the album apply tasks.
    fn run_pass_batch(cfg: &Config, payload: &TaskPayload) -> Result<LibraryStatus, String> {
        let eff = effective_config(cfg);
        let cfg = &eff;
        let library_id = payload.library_id;

        if !is_idle(cfg) {
            log_info("batch deferred: playback active; scheduling retry in 120s");
            post_webhook(cfg, &format!(
                "nd-organizer: batch {}/{} deferred - playback active.",
                payload.batch_index + 1,
                payload.batch_total.max(1)
            ));
            let _ = host::scheduler::schedule_one_time(120, "idle-retry", "");
            return Ok(LibraryStatus {
                library_id,
                batch_index: payload.batch_index,
                batch_total: payload.batch_total,
                ..Default::default()
            });
        }

        let lib = host::library::get_library(library_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("library {library_id} not found"))?;
        let root = Path::new(&lib.mount_point);
        if lib.mount_point.is_empty() {
            return Err("library has no filesystem mount: grant the library permission and write access (navidrome plugin edit nd-organizer --write-access)".into());
        }

        let mut plans = Vec::new();
        for dir in &payload.dirs {
            let album = discover_albums_skip(&root.join(dir), cfg.skip_hidden_files, &cfg.exclude_paths, 1, 0)
                .into_iter()
                .next();
            if let Some(album) = album {
                plans.push(build_plan(&album, cfg, root));
            }
        }
        let report = report::build_report(
            mode_label(cfg),
            &format!("{} (batch {}/{})", lib.name, payload.batch_index + 1, payload.batch_total.max(1)),
            &plans,
        );
        save_report(&report, cfg.backup_retention_days as i64);
        log_info(&report);
        post_webhook(cfg, &report);

        let status = LibraryStatus {
            library_id,
            name: lib.name,
            albums_found: plans.len(),
            albums_to_move: plans.iter().filter(|p| !p.moves.is_empty()).count(),
            file_moves: plans.iter().map(|p| p.moves.len()).sum(),
            kept: plans.iter().map(|p| p.keeps).sum(),
            skipped: plans.iter().map(|p| p.skipped.len()).sum(),
            batch_index: payload.batch_index,
            batch_total: payload.batch_total,
        };

        store::write_status(&status_json(
            cfg,
            false,
            std::slice::from_ref(&status),
            Some((payload.batch_index, payload.batch_total)),
        ));
        log_info(&format!(
            "STATUS: library={} mode={} batch={}/{} inProgress=false albumsToMove={} fileMoves={}",
            library_id,
            mode_label(cfg),
            payload.batch_index + 1,
            payload.batch_total.max(1),
            status.albums_to_move,
            status.file_moves
        ));

        if cfg.mode == Mode::Apply && !payload.dirs.is_empty() {
            let run_id = state::host_state::new_run_id();
            log_info(&format!("run {run_id}: enqueuing apply tasks for batch {} in library {library_id}", payload.batch_index + 1));
            for dir in &payload.dirs {
                if let Err(e) = enqueue("album", library_id, dir, &run_id) {
                    log_warn(&format!("enqueue apply {}: {e}", dir));
                }
            }
        }
        Ok(status)
    }

    /// Task worker: apply one album's plan. Rebuilds the plan at execution time
    /// so earlier tasks in the same pass are reflected.
    fn apply_one_album(cfg: &Config, library_id: i32, dir: &str, run_id: &str) -> Result<(), String> {
        let eff = effective_config(cfg);
        let cfg = &eff;
        let lib = host::library::get_library(library_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("library {library_id} not found"))?;
        let root = Path::new(&lib.mount_point);
        let album_path = root.join(dir);

        let album = discover_albums_skip(&album_path, cfg.skip_hidden_files, &cfg.exclude_paths, 0, 0)
            .into_iter()
            .next();
        let Some(album) = album else {
            log_info(&format!("apply: folder {} no longer exists, skipping", dir));
            return Ok(());
        };

        let plan = build_plan(&album, cfg, root);
        if plan.moves.is_empty() && plan.skipped.is_empty() && !cfg.write_nfo {
            log_info(&format!("apply: no changes for {}", dir));
            return Ok(());
        }
        let count = plan.moves.len();
        apply_plan(root, &plan, cfg.prune_empty_dirs)?;

        // Write/update NFO, backing up the previous content first so it can be
        // restored during a rollback (backups live in the KVStore).
        let mut nfo_written = None;
        let mut nfo_backup = None;
        if cfg.write_nfo {
            let nfo_path = root.join(&plan.target_dir).join("album.nfo");
            if cfg.backup_before_write && nfo_path.exists() {
                if let Ok(bytes) = std::fs::read(&nfo_path) {
                    let seq = state::host_state::next_seq(run_id).unwrap_or(0);
                    let key = state::backup_key(run_id, seq);
                    if store::save_backup(&key, bytes).is_ok() {
                        nfo_backup = Some(key);
                    }
                }
            }
            write_nfo_files(cfg, root, &plan);
            nfo_written = Some(format!("{}/album.nfo", plan.target_dir));
        }

        // Record the apply so it can be audited and rolled back.
        let mut rec = ApplyRecord {
            seq: 0,
            ts: state::now_ts(),
            run_id: run_id.to_string(),
            library_id,
            from_dir: plan.current_dir.clone(),
            to_dir: plan.target_dir.clone(),
            file_renames: plan
                .moves
                .iter()
                .map(|m| FileRename {
                    from: file_name(&m.from).to_string(),
                    to: file_name(&m.to).to_string(),
                })
                .collect(),
            dir_sidecars: plan.dir_sidecars.clone(),
            nfo_written,
            nfo_backup,
        };
        if let Err(e) = state::host_state::record_apply(&mut rec) {
            log_warn(&format!("record apply for {}: {e}", dir));
        }
        log_info(&format!(
            "applied: {} ({} file moves, seq {})",
            dir, count, rec.seq
        ));
        Ok(())
    }

    fn file_name(rel: &str) -> &str {
        rel.rsplit('/').next().unwrap_or(rel)
    }

    /// Rewrite album.nfo / artist.nfo from the metadata gathered so far. Phase 2
    /// feeds enriched (external) metadata into this same path.
    fn write_nfo_files(cfg: &Config, root: &Path, plan: &super::organizer::AlbumPlan) {
        let album_path = root.join(&plan.target_dir);
        let Some(album) = discover_albums_skip(&album_path, cfg.skip_hidden_files, &cfg.exclude_paths, 0, 0).into_iter().next() else {
            return;
        };
        let info = album_info_with_nfo(&album, cfg, root);
        let genre = if info.genre.is_empty() {
            vec![]
        } else {
            vec![info.genre.clone()]
        };
        let nfo_album = NfoAlbum {
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
        if let Err(e) = std::fs::write(album_path.join("album.nfo"), nfo::serialize_album(&nfo_album)) {
            log_warn(&format!("write album.nfo: {e}"));
        }
        // In the normal per-artist layout the parent folder is the artist folder.
        if plan.bucket == Bucket::Normal && !info.album_artist.is_empty() {
            if let Some(parent) = album_path.parent() {
                let nfo_artist = NfoArtist {
                    name: info.album_artist.clone(),
                    genres: genre,
                    ..Default::default()
                };
                if let Err(e) = std::fs::write(parent.join("artist.nfo"), nfo::serialize_artist(&nfo_artist)) {
                    log_warn(&format!("write artist.nfo: {e}"));
                }
            }
        }
    }

    fn save_report(report: &str, retention_days: i64) {
        store::write_report(report, retention_days);
    }

    /// Mirror a log line to the plugin's storage-dir log file (best-effort;
    /// the KVStore/report snapshots and Navidrome's server log always capture
    /// activity even when no plugin storage mount exists).
    fn file_log(level: &str, msg: &str) {
        store::append_log(level, msg);
    }

    fn mode_label(cfg: &Config) -> &'static str {
        match cfg.mode {
            Mode::DryRun => "dryRun",
            Mode::Apply => "apply",
        }
    }

    fn log_info(msg: &str) {
        extism_pdk::log!(extism_pdk::LogLevel::Info, "{}", msg);
        file_log("INFO", msg);
    }
    fn log_warn(msg: &str) {
        extism_pdk::log!(extism_pdk::LogLevel::Warn, "{}", msg);
        file_log("WARN", msg);
    }
    fn log_error(msg: &str) {
        extism_pdk::log!(extism_pdk::LogLevel::Error, "{}", msg);
        file_log("ERROR", msg);
    }

    register_lifecycle_init!(NdOrganizer);
    register_scheduler_callback!(NdOrganizer);
    register_taskworker_task_execute!(NdOrganizer);
}




