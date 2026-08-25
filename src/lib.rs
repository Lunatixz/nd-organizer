// nd-organizer — Navidrome music organizer plugin.
//
// Pure logic (config parsing, templates, classification, plan building) lives
// in modules that compile on the host so `cargo test` can exercise them. The
// Extism/Navidrome glue (capability exports, host-service calls) is gated to
// the wasm32 target, where the plugin actually runs.

// Phased development: some helpers are still only exercised by host tests.
#![allow(dead_code)]

mod config;
#[cfg(target_arch = "wasm32")]
mod artwork;
#[cfg(target_arch = "wasm32")]
mod audiomuse;
mod favorites;
mod identity;
mod lidarr;
#[cfg(target_arch = "wasm32")]
mod lyrics;
#[cfg(target_arch = "wasm32")]
mod musicbrainz;
mod nfo;
mod organizer;
mod report;
#[cfg(target_arch = "wasm32")]
mod scan;
mod state;
#[cfg(target_arch = "wasm32")]
mod trim;
mod stats;
#[cfg(target_arch = "wasm32")]
mod store;
mod tags;
mod template;
#[cfg(target_arch = "wasm32")]
mod net;

#[cfg(target_arch = "wasm32")]
pub(crate) mod wasm {
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
    use super::state::{self, ApplyRecord};
    use super::store;
    use super::template;

    const QUEUE: &str = "organize";
    const RUN_SCHEDULE_ID: &str = "nd-organizer-startup-run";

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct TaskPayload {
        /// "scan" = scan a chunk of the library; "group" = build album groups;
        /// "plan" = plan/apply a batch of album groups.
        kind: String,
        library_id: i32,
        dir: String,
        run_id: String,
        /// Album dirs / file lists carried by tasks.
        dirs: Vec<String>,
        /// Album groups (lists of file paths) for "plan" tasks.
        groups: Vec<Vec<String>>,
        batch_index: i32,
        batch_total: i32,
    }

    fn enqueue_payload(p: &TaskPayload) -> Result<(), String> {
        let bytes = serde_json::to_vec(p).map_err(|e| e.to_string())?;
        host::task::enqueue(QUEUE, bytes)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn enqueue(kind: &str, library_id: i32, dir: &str, run_id: &str) -> Result<(), String> {
        enqueue_payload(&TaskPayload {
            kind: kind.into(),
            library_id,
            dir: dir.into(),
            run_id: run_id.into(),
            dirs: vec![],
            groups: vec![],
            batch_index: -1,
            batch_total: -1,
        })
    }

    pub(crate) fn enqueue_scan_task(library_id: i32) -> Result<(), String> {
        enqueue("scan", library_id, "", "")
    }

    pub(crate) fn enqueue_group_task(library_id: i32) -> Result<(), String> {
        enqueue("group", library_id, "", "")
    }

    pub(crate) fn enqueue_cleanup_task(library_id: i32) -> Result<(), String> {
        enqueue("cleanup", library_id, "", "")
    }

    pub(crate) fn enqueue_plan_tasks(
        cfg: &Config,
        library_id: i32,
        groups: Vec<Vec<String>>,
    ) -> Result<usize, String> {
        let batch = cfg.albums_per_task.max(1);
        let total = groups.len().div_ceil(batch);
        let mut enqueued = 0;
        for (i, chunk) in groups.chunks(batch).enumerate() {
            enqueue_payload(&TaskPayload {
                kind: "plan".into(),
                library_id,
                dir: String::new(),
                run_id: String::new(),
                dirs: vec![],
                groups: chunk.to_vec(),
                batch_index: i as i32,
                batch_total: total as i32,
            })?;
            enqueued += 1;
        }
        Ok(enqueued)
    }

    /// The run id used for rollback of this pass's applies (stable per pass,
    /// shared by all plan tasks of a library).
    pub(crate) fn current_run_id(library_id: i32) -> Result<String, String> {
        let key = format!("run.current.{library_id}");
        if let Ok(Some(v)) = crate::store::kv().get(&key) {
            if let Ok(s) = String::from_utf8(v) {
                if !s.is_empty() {
                    return Ok(s);
                }
            }
        }
        let id = state::host_state::new_run_id();
        crate::store::kv().set(&key, id.clone().into_bytes()).map_err(|e| e.to_string())?;
        Ok(id)
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
            // Post an initial integration-health snapshot so the webhook
            // dashboard shows status even before the first scheduled pass.
            if !cfg.log_webhook_url.trim().is_empty() {
                let status = serde_json::json!({
                    "ts": state::now_ts(),
                    "mode": mode_label(&cfg),
                    "inProgress": false,
                    "integrations": integration_health(&cfg),
            "octoFiestaUrl": cfg.octo_fiesta_url,
            "octoFiestaProvider": cfg.octo_fiesta_provider,
                    "warnings": collect_warnings(&cfg),
                })
                .to_string();
                post_webhook(&cfg, &status);
            }
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
            if !cfg.schedule_cron.trim().is_empty() {
                if let Err(e) = host::scheduler::schedule_recurring(
                    &cfg.schedule_cron,
                    "organize",
                    "nd-organizer-cron",
                ) {
                    log_warn(&format!("schedule cron: {e}"));
                }
            }
            if cfg.playback_stats_enabled && cfg.stats_poll_minutes > 0 {
                let cron = format!("*/{} * * * *", cfg.stats_poll_minutes.min(60));
                if let Err(e) =
                    host::scheduler::schedule_recurring(&cron, "stats", "nd-organizer-stats")
                {
                    log_warn(&format!("schedule stats: {e}"));
                }
            }
            // Self-trim Navidrome's missing-files list on a schedule.
            if cfg.trim_missing_days > 0 {
                let cron = format!("0 4 */{} * *", cfg.trim_missing_days.min(30));
                if let Err(e) =
                    host::scheduler::schedule_recurring(&cron, "trim", "nd-organizer-trim")
                {
                    log_warn(&format!("schedule trim-missing: {e}"));
                }
            }
            Ok(())
        }
    }

    impl CallbackProvider for NdOrganizer {
        fn on_callback(
            &self,
            req: SchedulerCallbackRequest,
        ) -> Result<(), nd_pdk::scheduler::Error> {
            let cfg = Config::load().map_err(|e| nd_pdk::scheduler::Error::new(e))?;
            if req.payload == "stats" && cfg.playback_stats_enabled {
                if let Err(e) = enqueue("stats", 0, "", "") {
                    log_warn(&format!("enqueue stats: {e}"));
                }
                return Ok(());
            }
            if req.payload == "trim" && cfg.trim_missing_days > 0 {
                do_trim_missing(&cfg);
                return Ok(());
            }
            if req.payload == "rescan" {
                if let Err(e) = rescan_now(&cfg) {
                    log_warn(&format!("debounced rescan failed: {e}"));
                }
                return Ok(());
            }
            match run_pass(&cfg) {
                Ok(()) => log_info("run pass completed"),
                Err(e) => log_error(&e),
            }
            Ok(())
        }
    }

    impl TaskExecuteProvider for NdOrganizer {
        fn on_task_execute(
            &self,
            req: TaskExecuteRequest,
        ) -> Result<String, nd_pdk::taskworker::Error> {
            if req.queue_name != QUEUE {
                return Err(nd_pdk::taskworker::Error::new(format!(
                    "unknown queue {}",
                    req.queue_name
                )));
            }
            let payload: TaskPayload = serde_json::from_slice(&req.payload)
                .map_err(|e| nd_pdk::taskworker::Error::new(format!("bad payload: {e}")))?;
            let cfg = Config::load().map_err(|e| nd_pdk::taskworker::Error::new(e))?;
            let kind = payload.kind.as_str();
            record_task(kind, payload.library_id, "running", "");
            let r: Result<String, String> = match kind {
                "scan" => match super::scan::scan_step(&cfg, payload.library_id) {
                    Ok((outcome, n)) => {
                        let total: i64 = crate::store::kv()
                            .get(&format!("scan.count.{}", payload.library_id))
                            .ok()
                            .flatten()
                            .and_then(|v| String::from_utf8_lossy(&v).parse().ok())
                            .unwrap_or(n as i64);
                        Ok(match outcome {
                            super::scan::ScanOutcome::More => format!(
                                "scanned {n} files this chunk ({total} indexed so far - more remain)"
                            ),
                            super::scan::ScanOutcome::Done => format!(
                                "scan complete: {n} files in the final chunk ({total} indexed)"
                            ),
                            super::scan::ScanOutcome::Paused => format!(
                                "scan paused at maxScanEntries: {n} this chunk ({total} indexed - resumes next run)"
                            ),
                        })
                    }
                    Err(e) => Err(e),
                },
                "group" => super::scan::group_step(&cfg, payload.library_id)
                    .map(|(n, files)| format!("grouped {files} files into {n} plan tasks")),
                "plan" => super::scan::plan_step(
                    &cfg,
                    payload.library_id,
                    &payload.groups,
                    payload.batch_index,
                    payload.batch_total,
                )
                .map(|_| {
                    format!(
                        "plan batch {}/{} done",
                        payload.batch_index + 1,
                        payload.batch_total.max(1)
                    )
                }),
                "favsync" => crate::favorites::host_favorites::sync(&cfg).map(|s| {
                    format!(
                        "favorites sync: {} -> Last.fm, {} -> Navidrome, {} errors",
                        s.nav_to_lastfm, s.lastfm_to_nav, s.errors
                    )
                }),
                "cleanup" => super::scan::cleanup_step(&cfg, payload.library_id).map(|n| {
                    format!(
                        "cleanup: {n} no-audio folder(s) {}",
                        if cfg.mode == crate::config::Mode::Apply {
                            "deleted"
                        } else {
                            "would be deleted (dry-run)"
                        }
                    )
                }),
                "migrate" => match crate::store::migrate_mysql_chunk(&cfg, 400) {
                    Ok(crate::store::MigrateStatus::Copied(_)) => {
                        // More local keys to copy - keep going on the next pass.
                        if let Err(e) = enqueue("migrate", 0, "", "") {
                            log_warn(&format!("re-enqueue migrate: {e}"));
                        }
                        Ok("db migration in progress (batch copied)".into())
                    }
                    Ok(crate::store::MigrateStatus::Done) => Ok("db migration complete".into()),
                    Ok(crate::store::MigrateStatus::NoMysql) => Ok("not on mysql backend".into()),
                    Err(e) => Err(e),
                },
                "stats" => match crate::stats::host_stats::poll(&cfg) {
                    Ok(report) => {
                        let picks = if cfg.top_picks_count > 0 {
                            crate::stats::host_stats::refresh_top_picks(&cfg, cfg.top_picks_count)
                                .unwrap_or(0)
                        } else {
                            0
                        };
                        let filtered = crate::stats::host_stats::publish_filters(&cfg).unwrap_or(0);
                        // Pull star ratings from Navidrome before publishing
                        // outward, so manually-set ratings in the UI are
                        // captured into the plugin DB first.
                        let _pulled = crate::stats::host_stats::pull_navidrome_ratings(&cfg).unwrap_or(0);
                        let ratings = crate::stats::host_stats::publish_star_ratings(&cfg).unwrap_or(0);
                        let meta_writes = crate::stats::host_stats::write_playback_meta_tags(&cfg).unwrap_or(0);
                        let top_rated: Vec<serde_json::Value> = crate::stats::host_stats::top_rated(12)
                            .into_iter()
                            .map(|(name, path, stars, plays)| {
                                serde_json::json!({"name": name, "path": path, "stars": stars, "plays": plays})
                            })
                            .collect();
                        // Heartbeat: keep the webhook dashboard alive between runs
                        // (scan/batch status only posts on run events).
                        let heartbeat = serde_json::json!({
                            "ts": state::now_ts(),
                            "mode": mode_label(&cfg),
                            "inProgress": false,
                            "phase": "stats",
                            "plays": report.plays,
                            "skips": report.skips,
                            "topPicks": picks,
                            "filtered": filtered,
                            "ratings": ratings,
                            "metaWrites": meta_writes,
                            "nowPlaying": report.now_playing,
                            "topRated": top_rated,
                            "warnings": collect_warnings(&cfg),
                            "integrations": integration_health(&cfg),
            "octoFiestaUrl": cfg.octo_fiesta_url,
            "octoFiestaProvider": cfg.octo_fiesta_provider,
                            "tasks": task_log(),
                        })
                        .to_string();
                        post_webhook(&cfg, &heartbeat);
                        Ok(crate::stats::describe(&report, picks, filtered, ratings, meta_writes))
                    }
                    Err(e) => Err(e),
                },
                other => Err(format!("unknown task kind {other}")),
            };
            match &r {
                Ok(msg) => record_task(kind, payload.library_id, "done", msg),
                Err(e) => record_task(kind, payload.library_id, "failed", e),
            }
            r.map_err(|e| nd_pdk::taskworker::Error::new(e))
        }
    }

    /// Libraries to process: every library the plugin has been granted access
    /// to via the Navidrome "Library Access" permission (the check marks in the
    /// plugin settings) - the sole authority. `get_all_libraries` may list every
    /// library, but `get_library(id)` enforces the permission, so we filter to
    /// the genuinely accessible set. Legacy `libraries`/`libraryId` config values
    /// are ignored.
    pub(crate) fn target_libraries() -> Vec<i32> {
        let libs = match host::library::get_all_libraries() {
            Ok(libs) if !libs.is_empty() => libs,
            Ok(_) => {
                log_warn("no accessible libraries; grant Library Access in the plugin settings");
                return Vec::new();
            }
            Err(e) => {
                log_warn(&format!("cannot list libraries: {e}"));
                return Vec::new();
            }
        };
        // Only libraries the plugin can actually reach count - this is exactly
        // what the check marks in Library Access control.
        let targets: Vec<i32> = libs
            .into_iter()
            .filter(|l| host::library::get_library(l.id).ok().flatten().is_some())
            .map(|l| l.id)
            .collect();
        if targets.is_empty() {
            log_warn("no accessible libraries; grant Library Access in the plugin settings");
        }
        targets
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
    /// True when we may write tags for an artist's files. Lidarr-tracked artists
    /// are left untouched unless `writeTagsForTracked` is enabled (Lidarr stays
    /// the source of truth for its tracked artists).
    pub(crate) fn should_write_tags(cfg: &Config, artist: &str) -> bool {
        if cfg.lidarr_mode == crate::config::LidarrMode::Disabled || cfg.write_tags_for_tracked {
            return true;
        }
        !crate::lidarr::host_lidarr::artist_tracked(cfg, artist)
    }

    /// The user the plugin acts as for Navidrome calls. Prefers the configured
    /// `scanUser` when it's among the granted users; otherwise falls back to the
    /// first granted admin (then first granted user). When Navidrome exposes no
    /// user list, the configured scanUser is used as-is. This is what makes the
    /// plugin work when a single user is selected in User Access instead of
    /// requiring "all users".
    pub(crate) fn scan_user(cfg: &Config) -> String {
        let configured = cfg.scan_user.trim();
        match host::users::get_users() {
            Ok(users) if !users.is_empty() => {
                if !configured.is_empty() && users.iter().any(|u| u.user_name == configured) {
                    return configured.to_string();
                }
                if let Ok(admins) = host::users::get_admins() {
                    if let Some(a) = admins.first() {
                        if !a.user_name.is_empty() {
                            return a.user_name.clone();
                        }
                    }
                }
                if let Some(u) = users.first() {
                    if !u.user_name.is_empty() {
                        return u.user_name.clone();
                    }
                }
                configured.to_string()
            }
            _ => configured.to_string(),
        }
    }

    /// the Subsonic getNowPlaying API. Paused/stopped players count as idle -
    /// only entries whose `state` is "playing"/"starting" count as active.
    fn is_idle(cfg: &Config) -> bool {
        if !cfg.run_only_when_idle {
            return true;
        }
        let user = scan_user(cfg);
        if user.is_empty() {
            log_warn("runOnlyWhenIdle is on but no scan user is available; treating as idle");
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
                        matches!(
                            e.get("state").and_then(|s| s.as_str()),
                            Some("playing" | "starting")
                        )
                    })
                    .count();
                if active > 0 {
                    log_info(&format!(
                        "playback active ({active} playing); deferring organizer work"
                    ));
                }
                active == 0
            }
            Err(e) => {
                log_warn(&format!("idle check failed: {e}; proceeding"));
                true
            }
        }
    }

    /// First required external metadata provider that is currently unreachable.
    /// Only providers this config actually relies on for identification /
    /// album-track metadata count: AcoustID (verifyIdentity fingerprint),
    /// MusicBrainz (classification / genres / primary source), Lidarr (tracked
    /// artists). Artwork/lyrics/acoustic are enrichment - they stay fail-soft.
    /// Returns the provider name so the run can skip and retry later.
    fn required_meta_unreachable(cfg: &Config) -> Option<&'static str> {
        use crate::config::{AcoustIdMode, LidarrMode, PrimarySource};
        let empty = HashMap::new();
        if cfg.verify_identity
            && cfg.acoustid_mode != AcoustIdMode::Disabled
            && !cfg.acoustid_url.trim().is_empty()
        {
            let url = format!("{}/health", cfg.acoustid_url.trim_end_matches('/'));
            if !crate::net::circuit_check("acoustid", &url, &empty, 10_000) {
                return Some("AcoustID");
            }
        }
        let uses_mb = cfg.classify_from_mb
            || cfg.genre_from == "musicbrainz"
            || cfg.genre_from == "both"
            || cfg.primary_source == PrimarySource::MusicBrainz;
        if uses_mb {
            if !crate::net::circuit_check(
                "musicbrainz",
                "https://musicbrainz.org/ws/2/",
                &empty,
                15_000,
            ) {
                return Some("MusicBrainz");
            }
        }
        if cfg.lidarr_mode != LidarrMode::Disabled && !cfg.lidarr_url.trim().is_empty() {
            let mut h = HashMap::new();
            if !cfg.lidarr_api_key.trim().is_empty() {
                h.insert("X-Api-Key".to_string(), cfg.lidarr_api_key.clone());
            }
            // Resolve through the same candidates as the health check (host
            // LAN / docker host alias / subnet gateway) so a stale container
            // IP doesn't block runs when the port is host-published.
            let base = resolve_url_base(
                "lidarr",
                cfg.lidarr_url.trim_end_matches('/'),
                "/api/v1/system/status",
                &h,
            );
            let url = format!("{base}/api/v1/system/status");
            if !crate::net::circuit_check("lidarr", &url, &h, 10_000) {
                return Some("Lidarr");
            }
        }
        None
    }

    /// Build the activity status JSON persisted under `status:latest`.
    fn status_json(
        cfg: &Config,
        in_progress: bool,
        statuses: &[LibraryStatus],
        batch: Option<(i32, i32)>,
        run_id: Option<&str>,
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
            "runId": run_id,
            "batch": batch_field,
            "libraries": libs,
            "totalAlbumsToMove": total_to_move,
            "totalFileMoves": total_moves,
            "warnings": collect_warnings(cfg),
            "integrations": integration_health(cfg),
            "octoFiestaUrl": cfg.octo_fiesta_url,
            "octoFiestaProvider": cfg.octo_fiesta_provider,
            "persistenceBackend": cfg.persistence_backend,
            "tasks": task_log(),
        })
        .to_string()
    }

    /// Recent task executions (newest first), kept in the KVStore so status
    /// posts can show what the plugin is/was processing.
    fn record_task(kind: &str, library_id: i32, state: &str, message: &str) {
        let mut log: Vec<serde_json::Value> = crate::store::kv().get("tasklog")
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default();
        log.insert(
            0,
            serde_json::json!({
                "ts": state::now_ts(),
                "kind": kind,
                "libraryId": library_id,
                "state": state,
                "message": message,
            }),
        );
        log.truncate(20);
        if let Ok(bytes) = serde_json::to_vec(&log) {
            let _ = crate::store::kv().set("tasklog", bytes);
        }
    }

    pub(crate) fn task_log() -> serde_json::Value {
        crate::store::kv().get("tasklog")
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice::<serde_json::Value>(&v).ok())
            .unwrap_or_else(|| serde_json::json!([]))
    }

    /// Navidrome server's own host[:port], derived from its base URL config.
    /// Used to reach host-published sidecar ports (e.g. AudioMuse-AI's 8000)
    /// when a configured container IP is unreachable from inside the Navidrome
    /// container because they sit on different docker networks.
    pub(crate) fn server_host() -> Option<String> {
        let probe = |k: &str| -> Option<String> {
            host::config::get(k)
                .ok()
                .flatten()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .and_then(|v| host_of_baseurl(&v))
        };
        for key in ["BaseUrl", "baseurl", "URL", "url"] {
            if let Some(h) = probe(key) {
                return Some(h);
            }
        }
        if let Ok(keys) = host::config::keys("") {
            for k in keys {
                let l = k.to_lowercase();
                if l.ends_with("baseurl") || l == "url" || l.ends_with("base_url") {
                    if let Some(h) = probe(&k) {
                        return Some(h);
                    }
                }
            }
        }
        // Fall back to the bind address when it's a real LAN IP (not a wildcard).
        for key in ["Address", "address", "BindAddress"] {
            if let Ok(Some(v)) = host::config::get(key) {
                let v = v.trim().to_string();
                if !v.is_empty() && v != "0.0.0.0" && v != "::" && v != "localhost" {
                    return Some(v);
                }
            }
        }
        None
    }

    fn host_of_baseurl(baseurl: &str) -> Option<String> {
        let rest = baseurl.split("://").nth(1)?;
        let host_port = rest.split(['/', '?']).next()?.trim();
        // Host only - the caller appends the target service's own port.
        let host = host_port.split(':').next()?.trim();
        if host.is_empty() {
            None
        } else {
            Some(host.to_string())
        }
    }

    /// Probe a service URL (cached 1h) and return a warning when it's bad.
    pub(crate) fn probe_ok(
        key: &str,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Option<String> {
        probe_ok_timeout(key, url, headers, 5_000, 60)
    }

    /// Probe a service URL with an explicit timeout + cache TTL. Slow services
    /// (e.g. AudioMuse-AI, a heavy Flask app) get a longer timeout and a shorter
    /// cache so a transient slow response doesn't stick as "unreachable".
    pub(crate) fn probe_ok_timeout(
        key: &str,
        url: &str,
        headers: &HashMap<String, String>,
        timeout_ms: i32,
        cache_ttl: i64,
    ) -> Option<String> {
        let cache_key = format!("probe:{}:{}", key, url);
        if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
            let s = String::from_utf8_lossy(&v);
            return if s == "ok" {
                None
            } else {
                Some(s.into_owned())
            };
        }
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url: url.to_string(),
            headers: headers.clone(),
            no_follow_redirects: false,
            body: vec![],
            timeout_ms,
        };
        let result = match host::http::send(req) {
            // 2xx AND 3xx mean the server answered - a 302 redirect (e.g. a
            // login page) still proves the service is UP and reachable. Only a
            // transport failure (no response / refused / timeout) is "offline".
            Ok(Some(resp)) if resp.status_code < 400 => Ok(()),
            Ok(Some(resp)) if resp.status_code == 429 => Err(format!("rate limited (HTTP {})", resp.status_code)),
            Ok(Some(resp)) => Err(format!(
                "unreachable or wrong credentials (HTTP {})",
                resp.status_code
            )),
            Ok(None) => Err("no response".into()),
            Err(e) => {
                let msg = e.to_string();
                // EOF = connection closed immediately — likely transient, not permanent failure
                if msg.contains("EOF") {
                    Err("transient (EOF, retrying)".into())
                } else {
                    Err(format!("connection failed: {e}"))
                }
            }
        };
        // Don't cache transient errors (EOF, rate limit) — retry on next render
        let cache_ttl_actual = if result.is_err() { 30 } else { cache_ttl };
        let cached = match &result {
            Ok(()) => "ok".to_string(),
            Err(w) => format!("warn:{w}"),
        };
        let _ = crate::store::kv().set_with_ttl(&cache_key, cached.into_bytes(), cache_ttl_actual);
        match result {
            Ok(()) => None,
            Err(w) => Some(format!("{key}: {w}")),
        }
    }

    /// Health of every third-party integration, derived from the plugin config
    /// (kept in Navidrome settings, never in Docker). Included in status posts
    /// so the webhook dashboard can render it. Cached 5 min so it can't hammer
    /// the external APIs (it does real probes + a Last.fm login).
    pub(crate) fn integration_health(cfg: &Config) -> serde_json::Value {
        crate::net::cached("health", 60, || Some(integration_health_uncached(cfg)))
            .unwrap_or_else(|| serde_json::json!([]))
    }

    fn integration_health_uncached(cfg: &Config) -> serde_json::Value {
        use serde_json::json;
        let mut arr = Vec::new();

        let empty = HashMap::new();

        // 1. AcoustID sidecar (local Docker — fingerprinting)
        if cfg.acoustid_url.trim().is_empty() {
            arr.push(
                json!({"name":"AcoustID","state":"notConfigured","detail":"acoustidUrl not set"}),
            );
        } else {
            let url = format!("{}/health", cfg.acoustid_url.trim_end_matches('/'));
            match probe_ok("acoustid", &url, &empty) {
                None => arr.push(json!({"name":"AcoustID","state":"ok","detail":url})),
                Some(w) => arr.push(json!({"name":"AcoustID","state":"unreachable","detail":w})),
            }
        }

        // 2. Lidarr (local Docker — metadata + ratings)
        if cfg.lidarr_url.trim().is_empty() {
            arr.push(json!({"name":"Lidarr","state":"notConfigured","detail":"lidarrUrl not set"}));
        } else {
            let mut h = HashMap::new();
            if !cfg.lidarr_api_key.trim().is_empty() {
                h.insert("X-Api-Key".to_string(), cfg.lidarr_api_key.clone());
            }
            let base = resolve_url_base(
                "lidarr",
                cfg.lidarr_url.trim_end_matches('/'),
                "/api/v1/system/status",
                &h,
            );
            let url = format!("{base}/api/v1/system/status");
            match probe_ok("lidarr", &url, &h) {
                None => arr.push(json!({"name":"Lidarr","state":"ok","detail":url})),
                Some(w) => {
                    let state = if w.contains("HTTP 401") || w.contains("HTTP 403") {
                        "reachable"
                    } else {
                        "unreachable"
                    };
                    arr.push(json!({"name":"Lidarr","state":state,"detail":w}));
                }
            }
        }

        // 3. AudioMuse-AI (local Docker — acoustic analysis, heavy Flask app)
        if cfg.audiomuse_url.trim().is_empty() {
            arr.push(json!({"name":"AudioMuse-AI","state":"notConfigured","detail":"audiomuseUrl not set"}));
        } else {
            let url = crate::audiomuse::resolve_base(cfg);
            match probe_ok_timeout("audiomuse-v2", &url, &empty, 20_000, 60) {
                None => arr.push(json!({"name":"AudioMuse-AI","state":"ok","detail":url})),
                Some(w) => arr.push(json!({"name":"AudioMuse-AI","state":"unreachable","detail":w})),
            }
        }

        // 4. MusicBrainz (external API — metadata source, 1 req/s rate limit)
        {
            let url = "https://musicbrainz.org/ws/2/artist/?query=beatles&limit=1";
            let detail = if cfg.musicbrainz_token.trim().is_empty() {
                "no token (optional, 1 req/s)".to_string()
            } else {
                "token set".to_string()
            };
            let mut mb_headers = HashMap::new();
            mb_headers.insert("User-Agent".to_string(), "nd-organizer/0.2.0 (https://github.com/Lunatixz/nd-organizer)".to_string());
            // Cache 30s — metadata changes, 1 req/s rate limit
            match probe_ok_timeout("musicbrainz-hc", url, &mb_headers, 15_000, 30) {
                None => arr.push(json!({"name":"MusicBrainz","state":"ok","detail":detail})),
                Some(w) => arr.push(json!({"name":"MusicBrainz","state":"unreachable","detail":w})),
            }
        }

        // 5. ListenBrainz (external API — scrobble + ratings, same token as MB)
        if cfg.musicbrainz_token.trim().is_empty() {
            arr.push(json!({"name":"ListenBrainz","state":"notConfigured","detail":"set musicbrainzToken"}));
        } else {
            let mut lb_headers = HashMap::new();
            lb_headers.insert("User-Agent".to_string(), "nd-organizer/0.2.0 (https://github.com/Lunatixz/nd-organizer)".to_string());
            // Cache 30s — dynamic data
            match probe_ok_timeout("listenbrainz", "https://api.listenbrainz.org/1/", &lb_headers, 10_000, 30) {
                None => arr.push(json!({"name":"ListenBrainz","state":"ok","detail":"token set, api reachable"})),
                Some(w) => arr.push(json!({"name":"ListenBrainz","state":"unreachable","detail":w})),
            }
        }

        // 6. Last.fm (external API — scrobble + favorites)
        if cfg.lastfm_api_key.trim().is_empty() || cfg.lastfm_user.trim().is_empty() {
            arr.push(json!({"name":"Last.fm","state":"notConfigured","detail":"set lastfmApiKey + lastfmUser"}));
        } else if cfg.favorites_sync_lastfm
            && !cfg.lastfm_api_secret.trim().is_empty()
            && !cfg.lastfm_password.trim().is_empty()
        {
            match lastfm_auth_issue(cfg) {
                Some(issue) => arr.push(json!({"name":"Last.fm","state":"authFailed","detail":issue})),
                None => arr.push(json!({"name":"Last.fm","state":"ok","detail":"auth ok (favorites)"})),
            }
        } else {
            let url = format!(
                "https://ws.audioscrobbler.com/2.0/?method=user.getinfo&user={}&api_key={}&format=json",
                cfg.lastfm_user, cfg.lastfm_api_key
            );
            match probe_ok("lastfm", &url, &empty) {
                None => arr.push(json!({"name":"Last.fm","state":"ok","detail":"api reachable"})),
                Some(w) => arr.push(json!({"name":"Last.fm","state":"unreachable","detail":w})),
            }
        }

        // 8. Sidecars (local Docker containers — the plugin's own infrastructure)
        // Webhook is excluded — if you can see this dashboard, it's running.
        let sidecars = [
            ("nd-organizer-proxy", "Filter Proxy", 4534),
            ("nd-organizer-mysql", "MySQL", 8098),
            ("nd-organizer-radio", "Radio", 8100),
            ("nd-organizer-essentia", "Essentia", 8101),
        ];
        for (container, name, port) in sidecars {
            let url = format!("http://{container}:{port}/health");
            match probe_ok(container, &url, &empty) {
                None => arr.push(json!({"name":name,"state":"ok","detail":format!("port {port}")})),
                Some(w) => arr.push(json!({"name":name,"state":"unreachable","detail":w})),
            }
        }

        serde_json::json!(arr)
    }

    /// A human-readable Last.fm auth problem, or None when the login works.
    /// Cached 5 min (shared by health + warnings) so the auth endpoint is not
    /// hammered on every status post.
    fn lastfm_auth_issue(cfg: &Config) -> Option<String> {
        if !cfg.favorites_sync_lastfm {
            return None;
        }
        if cfg.lastfm_api_secret.trim().is_empty() || cfg.lastfm_password.trim().is_empty() {
            return None; // covered by a static config warning
        }
        crate::net::cached("lastfm-auth", 300, || {
            match crate::favorites::host_favorites::session(cfg) {
                Ok(_) => None,
                Err(e) => Some(format!("auth failed: {e}")),
            }
        })
    }

    /// Validate config + connectivity and return human-readable warnings shown
    /// in the status JSON so users notice wrong API keys or IP addresses.
    fn collect_warnings(cfg: &Config) -> Vec<String> {
        let mut w = Vec::new();
        if target_libraries().is_empty() {
            w.push(
                "no libraries are accessible - grant Library Access in the plugin permissions"
                    .into(),
            );
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
        if cfg.acoustid_mode != crate::config::AcoustIdMode::Disabled
            && cfg.acoustid_api_key.trim().is_empty()
        {
            w.push("acoustidApiKey is empty but acoustidMode is enabled".into());
        }
        if cfg.verify_identity && cfg.acoustid_url.trim().is_empty() {
            w.push("verifyIdentity is on but acoustidUrl is empty - files without an MBID/ISRC cannot be verified and will be routed to Singles. Deploy the AcoustID sidecar (acoustid/) and set its URL.".into());
        }
        if cfg.write_playcount
            && (cfg.lastfm_api_key.trim().is_empty() || cfg.lastfm_user.trim().is_empty())
        {
            w.push("writePlaycount requires lastfmApiKey and lastfmUser".into());
        }
        if cfg.lastfm_scrobble || cfg.lastfm_import_playcount {
            if cfg.lastfm_api_key.trim().is_empty()
                || cfg.lastfm_api_secret.trim().is_empty()
                || cfg.lastfm_password.trim().is_empty()
            {
                w.push("lastfmScrobble / lastfmImportPlaycount need lastfmApiKey + lastfmApiSecret + lastfmPassword (Last.fm session)".into());
            }
        }
        if cfg.favorites_sync_lastfm {
            if cfg.lastfm_api_secret.trim().is_empty() || cfg.lastfm_password.trim().is_empty() {
                w.push("favorites sync (Last.fm) is on but needs lastfmApiSecret + lastfmPassword (for the session key)".into());
            } else if let Some(issue) = lastfm_auth_issue(cfg) {
                w.push(format!(
                    "favorites sync (Last.fm) cannot log in: {issue}. Check lastfmPassword (and lastfmApiSecret)."
                ));
            }
            if scan_user(cfg).is_empty() {
                w.push("favorites sync needs a Navidrome user (grant one in User Access, or set scanUser)".into());
            }
        }
        if cfg.genre_from == "lastfm" && cfg.lastfm_api_key.trim().is_empty() {
            w.push("genreFrom is lastfm but lastfmApiKey is empty".into());
        }
        if cfg.use_lidarr_naming_schema
            && (cfg.lidarr_url.trim().is_empty() || cfg.lidarr_api_key.trim().is_empty())
        {
            w.push("useLidarrNamingSchema is on but Lidarr URL/API key are not configured".into());
        }
        if cfg.trigger_scan_after_run && scan_user(cfg).is_empty() {
            w.push("triggerScanAfterRun is on but no Navidrome user is available (grant one in User Access, or set scanUser)".into());
        }
        if !cfg.lidarr_url.trim().is_empty() {
            let mut h = HashMap::new();
            if !cfg.lidarr_api_key.trim().is_empty() {
                h.insert("X-Api-Key".to_string(), cfg.lidarr_api_key.clone());
            }
            let base = resolve_url_base(
                "lidarr",
                cfg.lidarr_url.trim_end_matches('/'),
                "/api/v1/system/status",
                &h,
            );
            if crate::net::throttle("warn-probe-lidarr", 60_000) {
                if let Some(war) = probe_ok("lidarr", &format!("{base}/api/v1/system/status"), &h) {
                    w.push(format!(
                        "Lidarr: {war}. If Lidarr runs in a Docker container, set lidarrUrl to its container name on the same network (e.g. http://lidarr:8686)."
                    ));
                }
            }
        }
        if !cfg.audiomuse_url.trim().is_empty() {
            let base = crate::audiomuse::resolve_base(cfg);
            if crate::net::throttle("warn-probe-audiomuse", 60_000) {
                if let Some(war) = probe_ok("audiomuse", &base, &HashMap::new()) {
                    w.push(format!(
                        "AudioMuse-AI: {war}. If AudioMuse-AI runs in a Docker container, set audiomuseUrl to its container name on the same network (e.g. http://audiomuse-ai-flask-app:8000)."
                    ));
                }
            }
        }
        w
    }

    /// POST a log/report body to the configured webhook (a hosted log).
    pub(crate) fn post_webhook(cfg: &Config, body: &str) {
        let url = cfg.log_webhook_url.trim();
        if url.is_empty() {
            return;
        }
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "text/plain".to_string());
        if !cfg.log_webhook_token.is_empty() {
            headers.insert("X-Token".to_string(), cfg.log_webhook_token.clone());
            headers.insert(
                "Authorization".to_string(),
                format!("Bearer {}", cfg.log_webhook_token),
            );
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

    /// Trigger a Navidrome rescan so the player never points at moved files.
    /// When `scanDebounceSeconds` > 0 the scan is scheduled instead of fired
    /// immediately, so bursts of changes coalesce into one rescan. Scans are
    /// incremental and also coalesced by Navidrome itself.
    pub(crate) fn trigger_navidrome_scan(cfg: &Config) -> Result<(), String> {
        if !cfg.trigger_scan_after_run {
            return Ok(());
        }
        if cfg.scan_debounce_seconds > 0 {
            let _ = host::scheduler::schedule_one_time(
                cfg.scan_debounce_seconds.max(1),
                "rescan",
                "nd-organizer-rescan",
            );
            return Ok(());
        }
        rescan_now(cfg)
    }

    /// Fire `startScan` immediately (used by the debounced callback too).
    fn rescan_now(cfg: &Config) -> Result<(), String> {
        let user = scan_user(cfg);
        if user.is_empty() {
            log_warn("triggerScanAfterRun is on but no scan user is available");
            return Ok(());
        }
        match host::subsonicapi::call(&format!("startScan?u={user}")) {
            Ok(json) => {
                let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
                let scanning = v
                    .pointer("/subsonic-response/scanStatus/scanning")
                    .and_then(|s| s.as_bool())
                    .unwrap_or(false);
                log_info(&format!("Navidrome scan triggered (scanning={scanning})"));
                Ok(())
            }
            Err(e) => Err(format!("startScan failed: {e}")),
        }
    }

    /// Purge Navidrome's missing-files list via its native API. Best-effort:
    /// logs the outcome and never fails the callback. Needs an admin account.
    fn do_trim_missing(cfg: &Config) {
        if cfg.trim_missing_days <= 0 {
            return;
        }
        let base = server_host()
            .map(|h| format!("http://{h}"))
            .unwrap_or_else(|| "http://navidrome:4533".to_string());
        let user = if cfg.navidrome_admin_user.trim().is_empty() {
            scan_user(cfg)
        } else {
            cfg.navidrome_admin_user.trim().to_string()
        };
        let pass = cfg.navidrome_admin_password.trim().to_string();
        if user.is_empty() || pass.is_empty() {
            log_warn(
                "trim-missing: need navidromeAdminUser/Password (admin) to purge missing files",
            );
            return;
        }
        match crate::trim::purge_missing(&base, &user, &pass) {
            Ok(n) => log_info(&format!(
                "trim-missing: purged missing-files list ({} entries reported removed)",
                n
            )),
            Err(e) => log_warn(&format!("trim-missing: {e}")),
        }
    }

    /// A scheduled pass. Handles a pending rollback first; otherwise, when the
    /// player is idle, kicks off a full-library scan -> group -> plan chain for
    /// each target library.
    fn run_pass(cfg: &Config) -> Result<(), String> {
        if !cfg.rollback_run_id.is_empty() {
            log_info(&format!(
                "rollback requested for run {}",
                cfg.rollback_run_id
            ));
            return do_rollback(cfg);
        }
        log_library_inventory();
        // If Navidrome's DB changed under us (fresh install / restored DB), the
        // cached scan index + star tallies + play/skip stats are stale - clear
        // them so we rebuild from the new library.
        super::scan::detect_db_change(cfg);
        // If the user switched persistenceBackend to mysql, copy the existing
        // local data over (chunked task; re-enqueues itself until done).
        if crate::store::mysql_migration_needed(cfg) {
            log_info("persistenceBackend=mysql: migrating existing local data to MySQL");
            if let Err(e) = enqueue("migrate", 0, "", "") {
                log_warn(&format!("enqueue migrate: {e}"));
            }
        }
        // Reset the per-run album budget so group_step can cap how many albums
        // this pass plans (maxAlbumsPerRun; 0 = unlimited).
        if cfg.max_albums_per_run > 0 {
            let _ = crate::store::kv().set(
                "run.albums.remaining",
                cfg.max_albums_per_run.to_string().into_bytes(),
            );
        }
        // Prune rollback data older than the retention window (default 30d).
        let pruned = crate::state::host_state::prune_rollback(cfg.rollback_retention_days as i64);
        if pruned > 0 {
            log_info(&format!("pruned {pruned} stale rollback keys (retention {}d)", cfg.rollback_retention_days));
        }
        let target_libs = target_libraries();
        if target_libs.is_empty() {
            log_error("nothing to organize: no libraries are accessible");
            store::write_status(&status_json(cfg, false, &[], None, None));
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
                "integrations": integration_health(cfg),
            "octoFiestaUrl": cfg.octo_fiesta_url,
            "octoFiestaProvider": cfg.octo_fiesta_provider,
                "tasks": task_log(),
            })
            .to_string();
            store::write_status(&status);
            post_webhook(
                cfg,
                &format!("nd-organizer: run deferred - playback active.\n{status}"),
            );
            let _ = host::scheduler::schedule_one_time(120, "idle-retry", "");
            return Ok(());
        }
        // A required external metadata provider (MusicBrainz / Lidarr /
        // AcoustID, per this config) being unreachable means we can't build
        // proper identities or album/track metadata. Skip the run and retry
        // later instead of organizing on degraded data. Gated by metaGateEnabled.
        if cfg.meta_gate_enabled {
            if let Some(provider) = required_meta_unreachable(cfg) {
                log_info(&format!(
                "run skipped: {provider} offline - retrying later (no actions without required metadata)"
            ));
            let status = serde_json::json!({
                "ts": state::now_ts(),
                "mode": mode_label(cfg),
                "inProgress": false,
                "metaSkipped": provider,
                "warnings": collect_warnings(cfg),
                "integrations": integration_health(cfg),
                "octoFiestaUrl": cfg.octo_fiesta_url,
                "octoFiestaProvider": cfg.octo_fiesta_provider,
                "tasks": task_log(),
            })
            .to_string();
            store::write_status(&status);
            post_webhook(
                cfg,
                &format!(
                    "nd-organizer: run skipped - {provider} is offline; retrying later so it never acts on missing metadata.\n{status}"
                ),
            );
            let _ = host::scheduler::schedule_one_time(300, "meta-retry", "");
            return Ok(());
            }
        }
        store::write_status(&status_json(cfg, true, &[], None, None));
        if cfg.favorites_sync_lastfm {
            if let Err(e) = enqueue("favsync", 0, "", "") {
                log_warn(&format!("enqueue favsync: {e}"));
            }
        }
        if cfg.mode == Mode::Apply {
            for &library_id in &target_libs {
                let _ = current_run_id(library_id);
            }
        }
        let mut enqueued = 0;
        for &library_id in &target_libs {
            // A fresh scan pass: reset the maxScanEntries per-pass counter so
            // the cap applies per run, not cumulatively forever.
            let _ = crate::store::kv().delete(&format!("scan.pass.{library_id}"));
            match enqueue_scan_task(library_id) {
                Ok(()) => enqueued += 1,
                Err(e) => log_warn(&format!("enqueue scan for library {library_id}: {e}")),
            }
        }
        log_info(&format!("run pass: enqueued {enqueued} scan tasks"));
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
        let target_libs = target_libraries();
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
            let recs: Vec<ApplyRecord> = recs
                .into_iter()
                .filter(|r| r.library_id == library_id)
                .collect();
            if !recs.is_empty() {
                if let Err(e) = state::host_state::run_rollback(root, &recs) {
                    errors.push(format!("library {library_id}: {e}"));
                } else {
                    log_info(&format!(
                        "library {library_id}: rolled back {} applies",
                        recs.len()
                    ));
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
            log_info(&format!(
                "rollback of run {run_id} complete. Clear the rollbackRunId setting."
            ));
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

    /// Replace the host part of a URL (used to try docker host aliases).
    pub(crate) fn host_replace(url: &str, new_host: &str) -> Option<String> {
        let idx = url.find("://")?;
        let scheme_end = idx + 3;
        let after = &url[scheme_end..];
        let path_start = after.find('/').unwrap_or(after.len());
        let authority = &after[..path_start];
        let path = &after[path_start..];
        let (_, port) = match authority.rsplit_once(':') {
            Some((_, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => ((), p),
            _ => ((), ""),
        };
        let new_authority = if port.is_empty() {
            new_host.to_string()
        } else {
            format!("{new_host}:{port}")
        };
        Some(format!("{}{}{}", &url[..scheme_end], new_authority, path))
    }

    /// Derive the subnet gateway of a URL's host (replace the last octet with 1,
    /// e.g. http://172.18.0.5:8000 -> http://172.18.0.1:8000). Docker's bridge
    /// gateway can reach host-published ports, so this helps reach services on
    /// the container's own network without any Docker changes.
    pub(crate) fn subnet_gateway(url: &str) -> Option<String> {
        let idx = url.find("://")? + 3;
        let after = &url[idx..];
        let path_start = after.find('/').unwrap_or(after.len());
        let authority = &after[..path_start];
        let (host, port) = authority.rsplit_once(':')?;
        let last_dot = host.rfind('.')?;
        if !host.chars().all(|c| c.is_ascii_digit() || c == '.') {
            return None;
        }
        Some(format!(
            "{}{}.1:{}{}",
            &url[..idx],
            &host[..last_dot],
            port,
            &after[path_start..]
        ))
    }

    /// Resolve the base URL to use for a service: the configured address first,
    /// then common docker host aliases (`host.docker.internal`, the default
    /// bridge gateway `172.17.0.1`) which reach host-published services when
    /// containers cannot reach the host's LAN IP. Returns the first that
    /// responds (probes are cached 1h).
    pub(crate) fn resolve_url_base(
        key: &str,
        configured: &str,
        probe_path: &str,
        headers: &HashMap<String, String>,
    ) -> String {
        let mut candidates = vec![configured.to_string()];
        // The Navidrome host's own LAN address (from its BaseUrl/address) can
        // reach host-published ports of containers on OTHER docker networks.
        if let Some(host) = server_host() {
            let port = configured
                .rsplit(':')
                .next()
                .map(|p| p.split('/').next().unwrap_or(p))
                .filter(|p| p.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or("");
            let alt = if port.is_empty() {
                configured
                    .rsplit_once(':')
                    .map(|(pre, _)| format!("{pre}:{host}"))
                    .unwrap_or_else(|| configured.to_string())
            } else {
                format!("http://{host}:{port}")
            };
            if !candidates.contains(&alt) {
                candidates.push(alt);
            }
        }
        for host in ["host.docker.internal", "172.17.0.1"] {
            if let Some(alt) = host_replace(configured, host) {
                if !candidates.contains(&alt) {
                    candidates.push(alt);
                }
            }
        }
        // Container name on the shared Docker network - once the user connects
        // Lidarr to the same network as Navidrome (docker network connect ...),
        // this resolves even though the configured static IP is stale.
        if key == "lidarr" {
            if let Some(alt) = host_replace(configured, "lidarr") {
                if !candidates.contains(&alt) {
                    candidates.push(alt);
                }
            }
        }
        // The subnet gateway of the configured address can reach host-published
        // ports on the container's own network (no Docker changes required).
        if let Some(gw) = subnet_gateway(configured) {
            if !candidates.contains(&gw) {
                candidates.push(gw);
            }
        }
        for c in candidates {
            if probe_ok(key, &format!("{c}{probe_path}"), headers).is_none() {
                return c;
            }
        }
        configured.to_string()
    }

    /// When `useLidarrNamingSchema` is on, fetch Lidarr's naming config and
    /// override folder/file schemas with it (cached for 7 days). Falls back to
    /// the plugin schemas when Lidarr is unreachable. Tries docker host aliases
    /// when the configured address is unreachable from the container.
    pub(crate) fn effective_config(cfg: &Config) -> Config {
        if !cfg.use_lidarr_naming_schema {
            return cfg.clone();
        }
        if cfg.lidarr_url.is_empty() || cfg.lidarr_api_key.is_empty() {
            log_warn("useLidarrNamingSchema is on but Lidarr URL/API key are not configured; using plugin schemas");
            return cfg.clone();
        }
        let configured_base = cfg.lidarr_url.trim_end_matches('/');
        let cache_key = format!("lidarr-naming:{configured_base}");
        if let Ok(Some(json)) = state::host_state::get_cached_meta("lidarr-naming", &cache_key) {
            if let Some(eff) = apply_lidarr_naming(cfg, &json) {
                return eff;
            }
        }
        let mut headers = HashMap::new();
        headers.insert("X-Api-Key".to_string(), cfg.lidarr_api_key.clone());
        let base = resolve_url_base("lidarr", configured_base, "/api/v1/system/status", &headers);
        if base != configured_base {
            log_info(&format!(
                "Lidarr reachable at {base} (container->host alias)"
            ));
        }
        let url = format!("{base}/api/v1/config/naming");
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
            Ok(Some(resp)) => log_warn(&format!(
                "Lidarr /config/naming returned HTTP {}",
                resp.status_code
            )),
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

    pub(crate) fn save_report(report: &str, retention_days: i64) {
        store::write_report(report, retention_days);
    }

    /// Mirror a log line to the plugin's storage-dir log file (best-effort;
    /// the KVStore/report snapshots and Navidrome's server log always capture
    /// activity even when no plugin storage mount exists).
    fn file_log(level: &str, msg: &str) {
        store::append_log(level, msg);
    }

    pub(crate) fn mode_label(cfg: &Config) -> &'static str {
        match cfg.mode {
            Mode::DryRun => "dryRun",
            Mode::Apply => "apply",
        }
    }

    pub(crate) fn log_info(msg: &str) {
        extism_pdk::log!(extism_pdk::LogLevel::Info, "{}", msg);
        file_log("INFO", msg);
    }
    pub(crate) fn log_warn(msg: &str) {
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

