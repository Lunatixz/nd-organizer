// Playback statistics + weighting.
//
// Tracks how often each song is PLAYED in full and how often it's SKIPPED, both
// observed from getNowPlaying transitions (works on older Navidrome that lacks
// the scrobbleretriever/users host services). A full play forgives a skip.
// Derives a weight and builds/updates a Navidrome playlist of the top picks so
// high-weight songs surface more often and frequently-skipped ones don't.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NowPlayingEntry {
    pub id: String,
    pub position_ms: i64,
    pub duration: i64,
}

/// A track played for less than `threshold_pct`% of its duration counts as a skip.
pub fn is_skip(duration_sec: i64, position_ms: i64, threshold_pct: i32) -> bool {
    if duration_sec <= 0 {
        return false;
    }
    let played_pct = (position_ms as f64) / ((duration_sec as f64) * 1000.0) * 100.0;
    played_pct < (threshold_pct.max(0) as f64)
}

/// Weight a song by plays vs skips. Skipped plays drag it down twice as hard.
pub fn weight(plays: i64, skips: i64) -> f64 {
    plays as f64 - 2.0 * skips as f64
}

/// True when a song should be hard-removed from playback: skipped strictly more
/// than it was ever played in full (net negative), past the ratio cap, with
/// enough samples. Songs you like but occasionally skip never hit this - they
/// only sink in priority via weight reordering.
pub fn hard_exclude(plays: i64, skips: i64, ratio: f64, min_samples: i64) -> bool {
    let total = plays + skips;
    plays < skips
        && total >= min_samples
        && (skips as f64) / (total as f64) >= ratio.clamp(0.0, 1.0)
}

/// A 1-5 enjoyment rating from plays/skips (for future tag writes).
pub fn rating_1_5(plays: i64, skips: i64) -> u8 {
    if plays <= 0 {
        return 0;
    }
    let ratio = plays as f64 / ((plays + skips).max(1) as f64);
    let r = (1.0 + 4.0 * ratio).round().clamp(1.0, 5.0);
    r as u8
}

/// Parse a Subsonic `getNowPlaying` response.
pub fn parse_nowplaying(json: &str) -> Vec<NowPlayingEntry> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let entries = v
        .pointer("/subsonic-response/nowPlaying/entry")
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default();
    entries
        .into_iter()
        .filter_map(|e| {
            let id = e.get("id")?.as_str()?.to_string();
            Some(NowPlayingEntry {
                id,
                position_ms: e.get("positionMs").and_then(|x| x.as_i64()).unwrap_or(0),
                duration: e.get("duration").and_then(|x| x.as_i64()).unwrap_or(0),
            })
        })
        .collect()
}

/// Extract the playlist id from a Subsonic `createPlaylist` response.
pub fn parse_playlist_id(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    v.pointer("/subsonic-response/playlist/id")
        .and_then(|i| i.as_str())
        .map(|s| s.to_string())
}

#[cfg(target_arch = "wasm32")]
pub mod host_stats {
    use crate::config::Config;
    use nd_pdk::host;

    use super::*;

    fn play_key(mfid: &str) -> String {
        format!("stat.play.{mfid}")
    }
    fn skip_key(mfid: &str) -> String {
        format!("stat.skip.{mfid}")
    }
    fn now_key(user: &str) -> String {
        format!("stat.now.{user}")
    }

    fn bump(key: &str, delta: i64) {
        let cur: i64 = crate::store::kv().get(key)
            .ok()
            .flatten()
            .and_then(|v| String::from_utf8_lossy(&v).parse().ok())
            .unwrap_or(0);
        let _ = crate::store::kv().set(key, (cur + delta).max(0).to_string().into_bytes());
    }

    /// Observe now-playing transitions to estimate skips AND full plays, without
    /// needing the scrobbleretriever/users host services (missing on older
    /// Navidrome). A track that leaves playback between polls is:
    ///   - a SKIP if it stopped before `threshold_pct`% of its duration;
    ///   - a FULL PLAY otherwise - which also forgives one previous skip.
    /// getNowPlaying returns every active session, so a single pass covers all
    /// users. Plays from observations are incremental (no historical scrobble
    /// ingestion), so weights build up over time on older hosts.
    fn observe(user: &str, threshold_pct: i32) -> Result<(usize, usize), String> {
        let uri = format!("getNowPlaying?u={user}");
        let json = host::subsonicapi::call(&uri).map_err(|e| e.to_string())?;
        let current = parse_nowplaying(&json);
        let key = now_key(user);
        let previous: Vec<NowPlayingEntry> = crate::store::kv().get(&key)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default();
        let mut plays = 0usize;
        let mut skips = 0usize;
        let current_ids: Vec<String> = current.iter().map(|e| e.id.clone()).collect();
        for prev in &previous {
            // A previously-playing track is no longer playing -> it ended.
            if !current_ids.contains(&prev.id) {
                if is_skip(prev.duration, prev.position_ms, threshold_pct) {
                    bump(&skip_key(&prev.id), 1);
                    skips += 1;
                } else {
                    // Played through the skip threshold: full play + forgiveness.
                    bump(&play_key(&prev.id), 1);
                    bump(&skip_key(&prev.id), -1);
                    plays += 1;
                }
            }
        }
        let _ = crate::store::kv().set(&key, serde_json::to_vec(&current).unwrap_or_default());
        Ok((plays, skips))
    }

    fn all_weights() -> Vec<(String, f64, i64, i64)> {
        let mut weights = Vec::new();
        if let Ok(keys) = crate::store::kv().list("stat.play.") {
            let vals = crate::store::kv().get_many(keys).unwrap_or_default();
            for (k, v) in vals {
                let mfid = k.strip_prefix("stat.play.").unwrap_or(&k).to_string();
                let plays = String::from_utf8_lossy(&v).parse::<i64>().unwrap_or(0);
                let skips = crate::store::kv().get(&skip_key(&mfid))
                    .ok()
                    .flatten()
                    .and_then(|v| String::from_utf8_lossy(&v).parse().ok())
                    .unwrap_or(0);
                weights.push((mfid, weight(plays, skips), plays, skips));
            }
        }
        weights.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        weights
    }

    /// Rebuild the "nd-organizer: Top Picks" Navidrome playlist from the weights.
    pub fn refresh_top_picks(cfg: &Config, count: usize) -> Result<usize, String> {
        let user = cfg.scan_user.trim();
        if user.is_empty() {
            return Err("scanUser needed for Top Picks playlist".into());
        }
        let weights = all_weights();
        let top: Vec<(String, f64)> = weights
            .into_iter()
            .take(count)
            .map(|(m, w, _, _)| (m, w))
            .collect();
        if top.is_empty() {
            return Ok(0);
        }
        let mut q = format!(
            "createPlaylist?name={}&u={user}",
            urlencode("nd-organizer: Top Picks")
        );
        for (mfid, _) in &top {
            q.push_str(&format!("&songId={}", urlencode(mfid)));
        }
        // Update an existing playlist if we've created one before.
        if let Some(id) = crate::store::kv().get("stat.playlist.id").ok().flatten() {
            if let Ok(id) = String::from_utf8(id) {
                if !id.is_empty() {
                    q.push_str(&format!("&playlistId={}", urlencode(&id)));
                }
            }
        }
        let resp = host::subsonicapi::call(&q).map_err(|e| e.to_string())?;
        if let Some(pid) = parse_playlist_id(&resp) {
            let _ = crate::store::kv().set("stat.playlist.id", pid.into_bytes());
        }
        Ok(top.len())
    }

    fn urlencode(s: &str) -> String {
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

    /// One stats pass: observe full plays + skips from now-playing transitions.
    /// No scrobbleretriever/users host services needed (older Navidrome).
    pub fn poll(cfg: &Config) -> Result<(usize, usize), String> {
        let user = cfg.scan_user.trim();
        if user.is_empty() {
            return Ok((0, 0));
        }
        observe(user, cfg.skip_threshold_percent)
    }

    /// Publish play/skip weights + frequently-skipped track IDs to the Subsonic
    /// filter proxy. The proxy reorders returned song lists by weight (so skipped
    /// tracks sink) and removes tracks past the skip cap. No files are moved.
    /// Opt-in (skipIgnoreEnabled + apply mode) and needs filterUrl configured.
    pub fn publish_filters(cfg: &Config) -> Result<usize, String> {
        use crate::config::Mode;
        use std::collections::HashMap;
        if !cfg.skip_ignore_enabled || cfg.mode != Mode::Apply {
            return Ok(0);
        }
        let base = cfg.filter_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return Ok(0);
        }
        let ratio = cfg.skip_ignore_ratio.clamp(0.0, 1.0);
        const MIN_SAMPLES: i64 = 3;
        let all = all_weights();
        // Hard-remove only when the song is a NET NEGATIVE: skipped strictly more
        // often than it was ever played in full AND its skip fraction passes the
        // cap. A song you like that you occasionally skip (plays >= skips) stays
        // hearable - it just sinks in priority via the weight reordering.
        let excluded: Vec<String> = all
            .iter()
            .filter(|(_, _, plays, skips)| hard_exclude(*plays, *skips, ratio, MIN_SAMPLES))
            .map(|(mfid, _, _, _)| mfid.clone())
            .collect();
        let weights: Vec<serde_json::Value> = all
            .into_iter()
            .map(|(mfid, w, plays, skips)| serde_json::json!([mfid, w, plays, skips]))
            .collect();
        // Push the Navidrome fillerKeywords setting so it drives the proxy's
        // queue filtering (single source of truth; FILTER_KEYWORDS env is just
        // the startup default).
        let keywords = crate::organizer::filler_keyword_list(cfg);
        let payload =
            serde_json::json!({ "excluded": excluded, "weights": weights, "keywords": keywords })
                .to_string();
        let req = host::http::HTTPRequest {
            method: "POST".into(),
            url: format!("{base}/filters"),
            headers: HashMap::from([("Content-Type".into(), "application/json".into())]),
            no_follow_redirects: false,
            body: payload.into_bytes(),
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if (200..300).contains(&resp.status_code) => {
                crate::wasm::log_info(&format!(
                    "published {} skip-heavy flags + {} weights + {} keywords to filter proxy at {base}",
                    excluded.len(),
                    weights.len(),
                    keywords.len()
                ));
                Ok(excluded.len())
            }
            Ok(Some(resp)) => Err(format!(
                "filter proxy {base} responded {}",
                resp.status_code
            )),
            Ok(None) => Err(format!("filter proxy {base} unreachable")),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_rule() {
        // 3-min song, played 30s -> skipped at 30% threshold.
        assert!(is_skip(180, 30_000, 30));
        // Played 90s of 180 -> not a skip.
        assert!(!is_skip(180, 90_000, 30));
        assert!(!is_skip(0, 10, 30));
    }

    #[test]
    fn weight_and_rating() {
        assert_eq!(weight(10, 0), 10.0);
        assert_eq!(weight(10, 9), -8.0);
        assert!(rating_1_5(10, 0) >= 4);
        assert_eq!(rating_1_5(0, 0), 0);
        assert!(rating_1_5(2, 8) < rating_1_5(8, 2));
    }

    #[test]
    fn hard_exclude_only_when_net_negative() {
        // Skipped more than played, past cap, enough samples -> excluded.
        assert!(hard_exclude(1, 3, 0.6, 3));
        assert!(hard_exclude(2, 3, 0.6, 3));
        // Played as much as skipped: you keep coming back -> never excluded.
        assert!(!hard_exclude(3, 3, 0.6, 3));
        // You like it: more full plays than skips -> never excluded.
        assert!(!hard_exclude(5, 2, 0.6, 3));
        assert!(!hard_exclude(3, 1, 0.6, 3));
        // Not enough samples yet.
        assert!(!hard_exclude(0, 2, 0.6, 3));
        // A full play forgives a skip: 2 plays/3 skips (excluded) -> play in
        // full once more -> 3 plays/2 skips (kept).
        assert!(hard_exclude(2, 3, 0.6, 3));
        assert!(!hard_exclude(3, 2, 0.6, 3));
    }

    #[test]
    fn parses_nowplaying_and_playlist_id() {
        let np = r#"{"subsonic-response":{"nowPlaying":{"entry":[
            {"id":"s1","positionMs":10000,"duration":180},
            {"id":"s2","positionMs":120000,"duration":240}
        ]}}}"#;
        let entries = parse_nowplaying(np);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].position_ms, 10_000);
        assert_eq!(entries[1].duration, 240);

        assert_eq!(
            parse_playlist_id(r#"{"subsonic-response":{"playlist":{"id":"pl-9"}}}"#),
            Some("pl-9".into())
        );
    }
}

