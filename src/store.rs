// Persistence for reports, logs and backups, plus the KVStore backend
// abstraction.
//
// The Navidrome host KVStore (per-plugin SQLite) is the default backend. When
// `persistenceBackend = mysql` + `persistenceUrl` are configured, the plugin's
// kvstore operations are routed to the mysql sidecar, which executes them
// against the user's MySQL/MariaDB database. The keys/values are identical, so
// the rest of the code is backend-agnostic.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use nd_pdk::host;
use serde_json::json;

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

/// Persistence backend.
#[derive(Clone)]
pub enum Kv {
    /// Navidrome-managed SQLite KVStore (default).
    Host,
    /// User's MySQL/MariaDB via the mysql sidecar.
    Mysql { url: String, db: MysqlDb },
}

#[derive(Clone)]
pub struct MysqlDb {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub user: String,
    pub password: String,
}

/// The active backend, chosen from the plugin config. Cached once so kvstore
/// operations never pay a per-op config load (a scan chunk does hundreds of
/// them). The backend only changes when the plugin is reloaded (Navidrome
/// re-instantiates the wasm module on a config save/rescan).
pub fn kv() -> &'static Kv {
    static BACKEND: OnceLock<Kv> = OnceLock::new();
    BACKEND.get_or_init(|| {
        let cfg = crate::config::Config::load().unwrap_or_default();
        if cfg.persistence_backend == "mysql" && !cfg.persistence_url.trim().is_empty() {
            Kv::Mysql {
                url: cfg.persistence_url.trim_end_matches('/').to_string(),
                db: MysqlDb {
                    host: cfg.mysql_host.clone(),
                    port: cfg.mysql_port,
                    name: cfg.mysql_name.clone(),
                    user: cfg.mysql_user.clone(),
                    password: cfg.mysql_password.clone(),
                },
            }
        } else {
            Kv::Host
        }
    })
}

impl Kv {
    /// POST an op to the mysql sidecar and return its `result` object.
    fn mysql_op(&self, op: &str, mut payload: serde_json::Value) -> Result<serde_json::Value, String> {
        let (url, db) = match self {
            Kv::Host => return Err("host backend has no mysql op".into()),
            Kv::Mysql { url, db } => (url.clone(), db),
        };
        payload["op"] = json!(op);
        payload["db"] = json!({
            "host": db.host,
            "port": db.port,
            "name": db.name,
            "user": db.user,
            "password": db.password,
        });
        let mut headers = HashMap::new();
        headers.insert("Content-Type".into(), "application/json".into());
        let req = host::http::HTTPRequest {
            method: "POST".into(),
            url: format!("{url}/kv"),
            headers,
            no_follow_redirects: false,
            body: payload.to_string().into_bytes(),
            timeout_ms: 15_000,
        };
        let resp = host::http::send(req).map_err(|e| e.to_string())?;
        let resp = resp.ok_or_else(|| "mysql sidecar: no response".to_string())?;
        if resp.status_code != 200 {
            return Err(format!("mysql sidecar HTTP {}", resp.status_code));
        }
        let v: serde_json::Value = serde_json::from_slice(&resp.body).map_err(|e| e.to_string())?;
        let result = v.get("result").cloned().unwrap_or_default();
        if let Some(err) = result.get("error").and_then(|e| e.as_str()) {
            return Err(err.to_string());
        }
        Ok(result)
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        match self {
            Kv::Host => host::kvstore::get(key).map_err(|e| e.to_string()),
            Kv::Mysql { .. } => {
                let r = self.mysql_op("get", json!({ "key": key }))?;
                if r.get("exists").and_then(|e| e.as_bool()).unwrap_or(false) {
                    let b64 = r.get("value").and_then(|v| v.as_str()).unwrap_or("");
                    BASE64.decode(b64).map(Some).map_err(|e| e.to_string())
                } else {
                    Ok(None)
                }
            }
        }
    }

    pub fn has(&self, key: &str) -> Result<bool, String> {
        match self {
            Kv::Host => host::kvstore::has(key).map_err(|e| e.to_string()),
            Kv::Mysql { .. } => self
                .mysql_op("has", json!({ "key": key }))
                .map(|r| r.get("exists").and_then(|e| e.as_bool()).unwrap_or(false)),
        }
    }

    pub fn set(&self, key: &str, value: Vec<u8>) -> Result<(), String> {
        match self {
            Kv::Host => host::kvstore::set(key, value).map_err(|e| e.to_string()),
            Kv::Mysql { .. } => self
                .mysql_op("set", json!({ "key": key, "value": BASE64.encode(value), "ttlSeconds": 0 }))
                .map(|_| ()),
        }
    }

    pub fn set_with_ttl(&self, key: &str, value: Vec<u8>, ttl_seconds: i64) -> Result<(), String> {
        match self {
            Kv::Host => host::kvstore::set_with_ttl(key, value, ttl_seconds).map_err(|e| e.to_string()),
            Kv::Mysql { .. } => self
                .mysql_op(
                    "set",
                    json!({ "key": key, "value": BASE64.encode(value), "ttlSeconds": ttl_seconds }),
                )
                .map(|_| ()),
        }
    }

    pub fn delete(&self, key: &str) -> Result<(), String> {
        match self {
            Kv::Host => host::kvstore::delete(key).map_err(|e| e.to_string()),
            Kv::Mysql { .. } => self.mysql_op("delete", json!({ "key": key })).map(|_| ()),
        }
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<String>, String> {
        match self {
            Kv::Host => host::kvstore::list(prefix).map_err(|e| e.to_string()),
            Kv::Mysql { .. } => self
                .mysql_op("list", json!({ "prefix": prefix }))
                .map(|r| {
                    r.get("keys")
                        .and_then(|k| k.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|k| k.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default()
                }),
        }
    }

    pub fn get_many(&self, keys: Vec<String>) -> Result<HashMap<String, Vec<u8>>, String> {
        match self {
            Kv::Host => host::kvstore::get_many(keys).map_err(|e| e.to_string()),
            Kv::Mysql { .. } => {
                let r = self.mysql_op("get_many", json!({ "keys": keys }))?;
                let mut out = HashMap::new();
                if let Some(map) = r.get("values").and_then(|m| m.as_object()) {
                    for (k, v) in map {
                        if let Some(b64) = v.as_str() {
                            if let Ok(bytes) = BASE64.decode(b64) {
                                out.insert(k.clone(), bytes);
                            }
                        }
                    }
                }
                Ok(out)
            }
        }
    }
}

/// Persist a run report: to the KVStore (durable, always) and to the storage
/// dir as a readable file (when available). Prunes old report entries beyond
/// the retention window. The latest report is always available under
/// `report:latest`.
pub fn write_report(report: &str, retention_days: i64) {
    let ts = crate::state::now_ts();
    let key = format!("report:{ts}");
    let _ = kv().set(&key, report.as_bytes().to_vec());
    let _ = kv().set("report:latest", report.as_bytes().to_vec());
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
    let _ = kv().set("status:latest", json.as_bytes().to_vec());
    if let Some(dir) = storage_dir() {
        let _ = std::fs::write(dir.join("status.json"), json);
    }
}

fn prune_reports(retention_days: i64) {
    let cutoff = crate::state::now_ts() - retention_days * 86_400;
    if let Ok(keys) = kv().list("report:") {
        for key in keys {
            let Some(ts) = key
                .strip_prefix("report:")
                .and_then(|s| s.parse::<i64>().ok())
            else {
                continue;
            };
            if ts < cutoff {
                let _ = kv().delete(&key);
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
    kv().set(key, bytes)
}
