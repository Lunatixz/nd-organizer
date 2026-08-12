// Persistence for reports, logs and backups.
//
// The plugin must work on Navidrome builds that predate the `storage` host
// service, so the KVStore is the primary, always-available store. When the host
// mounts the plugin storage dir at `/storage`, readable report/log files are
// also written there (best-effort).

use std::path::PathBuf;

use nd_pdk::host;

/// The plugin storage mount guest path, if the host provides it and it is
/// writable. Never calls the `storage` host service.
fn storage_dir() -> Option<PathBuf> {
    let p = PathBuf::from("/storage");
    if std::fs::create_dir_all(&p).is_err() {
        return None;
    }
    let probe = p.join(".nd-organizer-probe");
    match std::fs::write(&probe, b"1") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Some(p)
        }
        Err(_) => None,
    }
}

/// Persist a run report: to the KVStore (durable, always) and to the storage
/// dir as a readable file (when available). Prunes old report entries beyond
/// the retention window. The latest report is always available under
/// `report:latest`.
pub fn write_report(report: &str, retention_days: i64) {
    let ts = crate::state::now_ts();
    let key = format!("report:{ts}");
    let _ = host::kvstore::set(&key, report.as_bytes().to_vec());
    let _ = host::kvstore::set("report:latest", report.as_bytes().to_vec());
    if let Some(dir) = storage_dir() {
        let _ = std::fs::write(dir.join(format!("report-{ts}.txt")), report);
    }
    if retention_days > 0 {
        prune_reports(retention_days);
    }
}

/// Persist the latest run status snapshot (JSON) under `status:latest` and, when
/// available, as a readable `status.json` in the plugin storage dir.
pub fn write_status(json: &str) {
    let _ = host::kvstore::set("status:latest", json.as_bytes().to_vec());
    if let Some(dir) = storage_dir() {
        let _ = std::fs::write(dir.join("status.json"), json);
    }
}

fn prune_reports(retention_days: i64) {
    let cutoff = crate::state::now_ts() - retention_days * 86_400;
    if let Ok(keys) = host::kvstore::list("report:") {
        for key in keys {
            let Some(ts) = key
                .strip_prefix("report:")
                .and_then(|s| s.parse::<i64>().ok())
            else {
                continue;
            };
            if ts < cutoff {
                let _ = host::kvstore::delete(&key);
            }
        }
    }
    if let Some(dir) = storage_dir() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(ts) = name
                    .strip_prefix("report-")
                    .and_then(|s| s.strip_suffix(".txt"))
                    .and_then(|s| s.parse::<i64>().ok())
                {
                    if ts < cutoff {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
    }
}

/// Append a log line to the storage-dir log file (best-effort). The caller also
/// emits the line to the Navidrome server log via extism.
pub fn append_log(level: &str, msg: &str) {
    if let Some(dir) = storage_dir() {
        use std::io::Write;
        let path = dir.join("nd-organizer.log");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "[{}] [{}] {}", crate::state::now_ts(), level, msg);
        }
    }
}

/// Store a backup snapshot (original nfo/tag content) in the KVStore.
pub fn save_backup(key: &str, bytes: Vec<u8>) -> Result<(), String> {
    host::kvstore::set(key, bytes).map_err(|e| e.to_string())
}
