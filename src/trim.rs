// Navidrome "missing files" self-trimming.
//
// When a file is removed from disk, Navidrome marks it "missing" in its DB.
// This module periodically purges those entries via Navidrome's native REST API
// (`DELETE /missing` with no id = remove ALL missing files), so the missing-files
// list stays clean without manual cleanup. Admin-only on Navidrome's side, so it
// needs an admin username/password.

use std::collections::HashMap;

use base64::Engine as _;

#[cfg(target_arch = "wasm32")]
use nd_pdk::host;

/// Purge all missing files from Navidrome's DB via its native API.
/// `DELETE /missing` with no query deletes every missing entry. Best-effort:
/// returns the number purged or an error.
pub fn purge_missing(
    base: &str,
    username: &str,
    password: &str,
) -> Result<u64, String> {
    let url = format!("{}/api/missing", base.trim_end_matches('/'));
    let mut headers = HashMap::new();
    let auth = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
    );
    headers.insert("Authorization".to_string(), auth);
    let req = host::http::HTTPRequest {
        method: "DELETE".into(),
        url,
        headers,
        no_follow_redirects: false,
        body: vec![],
        timeout_ms: 30_000,
    };
    match host::http::send(req) {
        Ok(Some(resp)) if resp.status_code == 200 || resp.status_code == 204 => {
            // The API may return the deleted count in the body; fall back to unknown.
            let n = serde_json::from_slice::<serde_json::Value>(&resp.body)
                .ok()
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Ok(n)
        }
        Ok(Some(resp)) => Err(format!("navidrome DELETE /api/missing HTTP {}", resp.status_code)),
        Ok(None) => Err("navidrome DELETE /api/missing: no response".into()),
        Err(e) => Err(format!("navidrome DELETE /api/missing failed: {e}")),
    }
}
