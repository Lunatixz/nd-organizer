// Lidarr API client: album lookup, monitored check, and AlbumSearch trigger.
//
// Used by `lidarrForceSearchIncomplete`: only triggers a search when the artist
// AND album are both monitored in Lidarr and Lidarr lists more tracks than are
// present locally (i.e. the album is genuinely incomplete).

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LidarrAlbum {
    pub id: i64,
    pub title: String,
    pub artist: String,
    pub artist_id: i64,
    pub monitored: bool,
    pub artist_monitored: bool,
    /// Lidarr's expected track count for the album, if known.
    pub track_count: Option<i64>,
}

/// Extract an album from a Lidarr AlbumResource JSON object (pure, testable).
pub fn parse_album(val: &Value) -> Option<LidarrAlbum> {
    let id = val.get("id")?.as_i64()?;
    let title = val.get("title")?.as_str()?.to_string();
    let monitored = val
        .get("monitored")
        .and_then(|m| m.as_bool())
        .unwrap_or(false);
    let artist = val
        .get("artist")
        .and_then(|a| a.get("artistName"))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let artist_id = val
        .get("artistId")
        .or_else(|| val.get("artist").and_then(|a| a.get("id")))
        .and_then(|i| i.as_i64())
        .unwrap_or(0);
    let artist_monitored = val
        .get("artist")
        .and_then(|a| a.get("monitored"))
        .and_then(|m| m.as_bool())
        .unwrap_or(false);
    let track_count = val
        .get("statistics")
        .and_then(|s| s.get("trackCount"))
        .and_then(|t| t.as_i64());
    Some(LidarrAlbum {
        id,
        title,
        artist,
        artist_id,
        monitored,
        artist_monitored,
        track_count,
    })
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(target_arch = "wasm32")]
pub mod host_lidarr {
    use crate::config::Config;
    use std::collections::HashMap;

    use nd_pdk::host;

    use super::*;

    fn headers_with_key(cfg: &Config) -> HashMap<String, String> {
        let mut h = HashMap::new();
        h.insert("X-Api-Key".to_string(), cfg.lidarr_api_key.clone());
        h
    }

    fn base_url(cfg: &Config) -> String {
        crate::wasm::resolve_url_base(
            "lidarr",
            cfg.lidarr_url.trim_end_matches('/'),
            "/api/v1/system/status",
            &headers_with_key(cfg),
        )
    }

    /// Circuit-aware HTTP send. Distinguishes OFFLINE (transport failure / no
    /// response / 5xx) from a live API that just returned no data (4xx/404):
    /// only offline trips the circuit. 2xx clears it.
    fn lidarr_send(
        cfg: &Config,
        method: &str,
        url: String,
        body: Vec<u8>,
    ) -> Result<Option<host::http::HTTPResponse>, String> {
        if !crate::net::circuit_probe(
            "lidarr",
            &format!("{}/api/v1/system/status", base_url(cfg)),
            &headers_with_key(cfg),
            10_000,
        ) {
            return Err("Lidarr offline (cooldown)".into());
        }
        let mut headers = headers_with_key(cfg);
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        let req = host::http::HTTPRequest {
            method: method.into(),
            url,
            headers,
            no_follow_redirects: false,
            body,
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if (200..300).contains(&resp.status_code) => {
                crate::net::circuit_clear("lidarr");
                Ok(Some(resp))
            }
            Ok(Some(resp)) if resp.status_code >= 500 => {
                crate::net::circuit_mark_failed("lidarr");
                Ok(Some(resp))
            }
            Ok(other) => Ok(other), // 4xx / 404 / 401 -> live API, no data
            Err(e) => {
                crate::net::circuit_mark_failed("lidarr");
                Err(e.to_string())
            }
        }
    }

    /// Find a Lidarr album by title+artist. Cached for 24h (including "not
    /// found"). Fetches the album detail for the track count when the lookup
    /// does not include statistics.
    pub fn find_album(cfg: &Config, album: &str, artist: &str) -> Option<LidarrAlbum> {
        if cfg.lidarr_url.trim().is_empty() || cfg.lidarr_api_key.trim().is_empty() {
            return None;
        }
        let cache_key = format!(
            "lidarr-album:{}|{}",
            artist.to_lowercase(),
            album.to_lowercase()
        );
        if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
            if let Ok(val) = serde_json::from_slice::<Value>(&v) {
                return parse_album(&val);
            }
        }
        let base = base_url(cfg);
        let url = format!(
            "{base}/api/v1/album/lookup?term={}",
            url_encode(&format!("{artist} {album}"))
        );
        let mut found: Option<LidarrAlbum> = match lidarr_send(cfg, "GET", url, vec![]) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                let body = String::from_utf8_lossy(&resp.body).into_owned();
                serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.as_array().cloned())
                    .and_then(|items| {
                        items.iter().find_map(|it| {
                            let a = parse_album(it)?;
                            if a.title.eq_ignore_ascii_case(album)
                                && a.artist.eq_ignore_ascii_case(artist)
                            {
                                Some(a)
                            } else {
                                None
                            }
                        })
                    })
            }
            _ => None,
        };
        // If the lookup lacks statistics, fetch the album detail for the count.
        if let Some(a) = &found {
            if a.track_count.is_none() {
                let url = format!("{base}/api/v1/album/{}", a.id);
                if let Ok(Some(resp)) = lidarr_send(cfg, "GET", url, vec![]) {
                    if resp.status_code == 200 {
                        if let Ok(v) = serde_json::from_slice::<Value>(&resp.body) {
                            if let Some(mut a2) = parse_album(&v) {
                                a2.track_count = v
                                    .get("statistics")
                                    .and_then(|s| s.get("trackCount"))
                                    .and_then(|t| t.as_i64())
                                    .or(a.track_count);
                                found = Some(a2);
                            }
                        }
                    }
                }
            }
        }
        let cached = match &found {
            Some(a) => serde_json::to_value(a).unwrap_or_else(|_| Value::Null),
            None => Value::Null,
        };
        let _ = crate::store::kv().set_with_ttl(&cache_key, cached.to_string().into_bytes(), 86_400);
        found
    }

    /// True when the album is incomplete AND both the artist and album are
    /// monitored in Lidarr. Returns the Lidarr album id when a search should run.
    pub fn incomplete_monitored(
        cfg: &Config,
        local_track_count: usize,
        album: &str,
        artist: &str,
    ) -> Option<i64> {
        if artist.trim().is_empty() || album.trim().is_empty() {
            return None;
        }
        let a = find_album(cfg, album, artist)?;
        if !(a.monitored && a.artist_monitored) {
            return None;
        }
        match a.track_count {
            Some(tc) if tc > local_track_count as i64 => Some(a.id),
            _ => None,
        }
    }

    /// Is this artist tracked in Lidarr? Cached 10 min so tag-write gating
    /// doesn't hammer Lidarr once per album.
    pub fn artist_tracked(cfg: &Config, artist: &str) -> bool {
        let artist = artist.trim();
        if artist.is_empty()
            || cfg.lidarr_url.trim().is_empty()
            || cfg.lidarr_api_key.trim().is_empty()
        {
            return false;
        }
        crate::net::cached(&format!("lidarr-artist:{}", artist.to_lowercase()), 600, || {
            let base = base_url(cfg);
            let url = format!("{base}/api/v1/artist/search?term={}", url_encode(artist));
            let found = match lidarr_send(cfg, "GET", url, vec![]) {
                Ok(Some(resp)) if resp.status_code == 200 => {
                    let body = String::from_utf8_lossy(&resp.body).into_owned();
                    serde_json::from_str::<Value>(&body)
                        .ok()
                        .and_then(|v| v.as_array().cloned())
                        .map(|items| {
                            items.iter().any(|a| {
                                a.get("artistName")
                                    .and_then(|n| n.as_str())
                                    .map(|n| n.eq_ignore_ascii_case(artist))
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                }
                _ => false,
            };
            Some(found)
        })
        .unwrap_or(false)
    }

    /// Trigger a Lidarr AlbumSearch for the given album id (searches only the
    /// missing tracks).
    pub fn force_search(cfg: &Config, album_id: i64) -> Result<(), String> {
        send_command(cfg, &format!("{{\"name\":\"AlbumSearch\",\"albumIds\":[{album_id}]}}"))
    }

    /// Tell Lidarr to refresh an artist so its file paths stay in sync after the
    /// organizer moves files. Only meaningful for artists Lidarr tracks.
    pub fn refresh_artist(cfg: &Config, artist_id: i64) -> Result<(), String> {
        send_command(cfg, &format!("{{\"name\":\"RefreshArtist\",\"artistIds\":[{artist_id}]}}"))
    }

    /// POST a command body to Lidarr's /api/v1/command endpoint.
    fn send_command(cfg: &Config, body: &str) -> Result<(), String> {
        let base = base_url(cfg);
        match lidarr_send(cfg, "POST", format!("{base}/api/v1/command"), body.as_bytes().to_vec()) {
            Ok(Some(resp)) if resp.status_code < 300 => Ok(()),
            Ok(Some(resp)) => Err(format!("Lidarr command returned HTTP {}", resp.status_code)),
            Ok(None) => Err("no response".into()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_album_resource() {
        let v = json!({
            "id": 42,
            "title": "The Wall",
            "monitored": true,
            "artistId": 99,
            "artist": { "id": 99, "artistName": "Pink Floyd", "monitored": true },
            "statistics": { "trackCount": 26 }
        });
        let a = parse_album(&v).unwrap();
        assert_eq!(a.id, 42);
        assert_eq!(a.title, "The Wall");
        assert_eq!(a.artist, "Pink Floyd");
        assert_eq!(a.artist_id, 99);
        assert!(a.monitored && a.artist_monitored);
        assert_eq!(a.track_count, Some(26));
    }

    #[test]
    fn parses_unmonitored_and_missing_stats() {
        let v = json!({
            "id": 7,
            "title": "EP",
            "monitored": false,
            "artist": { "id": 12, "artistName": "Someone", "monitored": true }
        });
        let a = parse_album(&v).unwrap();
        assert!(!a.monitored);
        assert_eq!(a.track_count, None);
        // artistId falls back to artist.id.
        assert_eq!(a.artist_id, 12);
    }

    #[test]
    fn url_encodes_query() {
        assert_eq!(
            url_encode("Pink Floyd The Wall"),
            "Pink%20Floyd%20The%20Wall"
        );
        assert_eq!(url_encode("a&b=c"), "a%26b%3Dc");
    }
}

