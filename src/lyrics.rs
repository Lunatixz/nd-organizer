// Synchronized lyrics via LRCLIB (lrclib.net) - free, no API key, the de-facto
// source for LRC lyrics. Fetches per track (cached 7 days, throttled ~1 req/s)
// and writes an .lrc / .txt sidecar next to the track.

use std::collections::HashMap;
use std::path::Path;

use nd_pdk::host;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Lyrics {
    #[serde(default)]
    pub synced: Option<String>,
    #[serde(default)]
    pub plain: Option<String>,
}

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Look up a track's lyrics on LRCLIB. Cached 7 days; None when there are none.
pub fn fetch(artist: &str, title: &str, album: &str, duration_secs: i64) -> Option<Lyrics> {
    if artist.trim().is_empty() || title.trim().is_empty() {
        return None;
    }
    let cache_key = format!("lyr:{}|{}", artist.to_lowercase(), title.to_lowercase());
    if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
        if let Ok(l) = serde_json::from_slice::<Lyrics>(&v) {
            return Some(l);
        }
    }
    if !crate::net::circuit_probe("lrclib", "https://lrclib.net/api", &HashMap::new(), 10_000) {
        return None; // offline - fail fast (auto-recovers via probe)
    }
    if !crate::net::throttle("lrclib", 1000) {
        return None;
    }
    let mut url = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}",
        urlenc(artist),
        urlenc(title)
    );
    if !album.trim().is_empty() {
        url.push_str(&format!("&album_name={}", urlenc(album)));
    }
    if duration_secs > 0 {
        url.push_str(&format!("&duration={duration_secs}"));
    }
    let req = host::http::HTTPRequest {
        method: "GET".into(),
        url,
        headers: HashMap::new(),
        no_follow_redirects: false,
        body: vec![],
        timeout_ms: 10_000,
    };
    let lyrics: Option<Lyrics> = match host::http::send(req) {
        Ok(Some(resp)) if resp.status_code == 200 => {
            crate::net::circuit_clear("lrclib");
            let Ok(v) = serde_json::from_slice::<Value>(&resp.body) else {
                return None;
            };
            let synced = v
                .get("syncedLyrics")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let plain = v
                .get("plainLyrics")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            if synced.is_none() && plain.is_none() {
                None
            } else {
                Some(Lyrics { synced, plain })
            }
        }
        Ok(Some(resp)) if resp.status_code == 404 => None, // no lyrics - not an outage
        Ok(Some(_)) | Ok(None) | Err(_) => {
            crate::net::circuit_mark_failed("lrclib");
            None
        }
    };
    if let Some(l) = &lyrics {
        let _ = crate::store::kv().set_with_ttl(
            &cache_key,
            serde_json::to_vec(l).unwrap_or_default(),
            7 * 24 * 3600,
        );
    }
    lyrics
}

/// Write the lyrics sidecar next to a track. `format` = "lrc" (synced) or "txt"
/// (plain). Skips when the sidecar already exists.
pub fn write_sidecar(path: &Path, lyrics: &Lyrics, format: &str) -> Result<(), String> {
    let content = if format == "txt" {
        lyrics.plain.clone().or_else(|| lyrics.synced.clone())
    } else {
        lyrics.synced.clone().or_else(|| lyrics.plain.clone())
    };
    let content = content.ok_or("no lyrics to write")?;
    let ext = if format == "txt" { "txt" } else { "lrc" };
    let out = path.with_extension(ext);
    if out.exists() {
        return Ok(());
    }
    std::fs::write(&out, content).map_err(|e| e.to_string())
}
