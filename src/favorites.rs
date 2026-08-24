// Favorites sync: Navidrome (the hub, Subsonic stars) <-> Last.fm (loved tracks).
//
// Any Subsonic-compatible client stores its favorites as server-side stars in
// Navidrome, so it participates automatically. MusicBrainz has no favorites API;
// its MBIDs are used only as identity keys to match tracks across platforms.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StarredSong {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub mbid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LovedTrack {
    pub title: String,
    pub artist: String,
    pub mbid: String,
}

/// Normalized match key: lowercased artist|title.
pub fn match_key(title: &str, artist: &str) -> String {
    format!(
        "{}|{}",
        artist.trim().to_ascii_lowercase(),
        title.trim().to_ascii_lowercase()
    )
}

/// Two entries are the same track when the recording MBID matches, or (as a
/// fallback) the normalized artist|title match.
pub fn same_track(title: &str, artist: &str, mbid: &str, s: &StarredSong) -> bool {
    if !mbid.trim().is_empty() && !s.mbid.trim().is_empty() {
        return mbid.trim().eq_ignore_ascii_case(&s.mbid);
    }
    match_key(title, artist) == match_key(&s.title, &s.artist)
}

/// Parse a Subsonic `getStarred2` response into songs.
pub fn parse_starred(json: &str) -> Vec<StarredSong> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let songs = v
        .pointer("/subsonic-response/starred2/song")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    songs
        .into_iter()
        .filter_map(|s| {
            let id = s.get("id")?.as_str()?.to_string();
            Some(StarredSong {
                id,
                title: s
                    .get("title")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                artist: s
                    .get("artist")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                album: s
                    .get("album")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                mbid: s
                    .get("musicBrainzId")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// Parse a Last.fm `user.getLovedTracks` response.
pub fn parse_loved(json: &str) -> Vec<LovedTrack> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let tracks = v
        .pointer("/lovedtracks/track")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    tracks
        .into_iter()
        .filter_map(|t| {
            let title = t.get("name")?.as_str()?.to_string();
            let artist = t
                .get("artist")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            Some(LovedTrack {
                title,
                artist,
                mbid: t
                    .get("mbid")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct SyncSummary {
    pub nav_to_lastfm: usize,
    pub lastfm_to_nav: usize,
    pub errors: usize,
}

#[cfg(target_arch = "wasm32")]
pub mod host_favorites {
    use std::collections::HashMap;

    use crate::config::Config;
    use md5::{Digest, Md5};
    use nd_pdk::host;

    use super::*;

    fn md5hex(s: &[u8]) -> String {
        let mut h = Md5::new();
        h.update(s);
        format!("{:x}", h.finalize())
    }

    pub(crate) fn urlencode(s: &str) -> String {
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

    /// Last.fm method signature: md5 of sorted "namevalue" params (excluding
    /// `api_sig` and `format`) concatenated with the API secret.
    fn api_sig(secret: &str, params: &mut Vec<(String, String)>) -> String {
        params.retain(|(k, _)| k != "api_sig" && k != "format");
        params.sort();
        let s: String = params
            .iter()
            .map(|(k, v)| format!("{k}{v}"))
            .collect::<String>()
            + secret;
        md5hex(s.as_bytes())
    }

    fn lastfm_get(cfg: &Config, method: &str, params: &[(&str, &str)]) -> Option<Value> {
        if !crate::net::circuit_probe(
            "lastfm",
            "https://ws.audioscrobbler.com/2.0/",
            &HashMap::new(),
            10_000,
        ) {
            return None; // offline - fail fast (auto-recovers via probe)
        }
        let mut q = format!("method={method}&api_key={}&format=json", cfg.lastfm_api_key);
        for (k, v) in params {
            q.push_str(&format!("&{k}={}", urlencode(v)));
        }
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url: format!("https://ws.audioscrobbler.com/2.0/?{q}"),
            headers: HashMap::new(),
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                crate::net::circuit_clear("lastfm");
                serde_json::from_slice::<Value>(&resp.body).ok()
            }
            Ok(Some(_)) => None, // API error (auth etc.) - live, not an outage
            Ok(None) | Err(_) => {
                crate::net::circuit_mark_failed("lastfm");
                None
            }
        }
    }

    fn lastfm_post(
        cfg: &Config,
        sk: &str,
        method: &str,
        params: &[(&str, &str)],
    ) -> Result<Value, String> {
        let mut ps: Vec<(String, String)> = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        ps.push(("api_key".into(), cfg.lastfm_api_key.clone()));
        ps.push(("method".into(), method.to_string()));
        if !sk.is_empty() {
            ps.push(("sk".into(), sk.to_string()));
        }
        let sig = api_sig(&cfg.lastfm_api_secret, &mut ps);
        ps.push(("api_sig".into(), sig));
        ps.push(("format".into(), "json".into()));
        let body = ps
            .iter()
            .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let mut headers = HashMap::new();
        headers.insert(
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        );
        let req = host::http::HTTPRequest {
            method: "POST".into(),
            url: "https://ws.audioscrobbler.com/2.0/".into(),
            headers,
            no_follow_redirects: false,
            body: body.into_bytes(),
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                crate::net::circuit_clear("lastfm");
                serde_json::from_slice::<Value>(&resp.body).map_err(|e| e.to_string())
            }
            Ok(Some(resp)) => {
                // Last.fm replies with XML error bodies (e.g. code 4 "Authentication
                // Failed") - surface them so a bad API key/password is obvious.
                // An API error means Last.fm is LIVE, not an outage - no circuit trip.
                let hint = String::from_utf8_lossy(&resp.body)
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(160)
                    .collect::<String>();
                Err(format!("Last.fm HTTP {}: {}", resp.status_code, hint.trim()))
            }
            Ok(None) => {
                crate::net::circuit_mark_failed("lastfm");
                Err("Last.fm no response".into())
            }
            Err(e) => {
                crate::net::circuit_mark_failed("lastfm");
                Err(format!("Last.fm request failed: {e}"))
            }
        }
    }

    /// Obtain (and cache) the Last.fm session key via auth.getMobileSession.
    ///
    /// Hard cooldown on the actual auth POST so repeated runs/tasks never
    /// hammer Last.fm (repeated failures can also trigger a temporary account
    /// lockout). While throttled it returns the last recorded error instead.
    pub(crate) fn session(cfg: &Config) -> Result<String, String> {
        if let Ok(Some(v)) = crate::store::kv().get("lastfm.sk") {
            if let Ok(s) = String::from_utf8(v) {
                if !s.is_empty() {
                    return Ok(s);
                }
            }
        }
        if cfg.lastfm_api_secret.is_empty() || cfg.lastfm_password.is_empty() {
            return Err(
                "favorites sync needs lastfmApiSecret + lastfmPassword (for the session key)"
                    .into(),
            );
        }
        // Cooldown between real auth attempts (even across callers).
        const AUTH_COOLDOWN_SECS: i64 = 5 * 60;
        let now = crate::state::now_ts();
        if let Ok(Some(v)) = crate::store::kv().get("lastfm.auth_attempt") {
            if let Ok(last) = String::from_utf8_lossy(&v).parse::<i64>() {
                if now - last < AUTH_COOLDOWN_SECS {
                    let wait = AUTH_COOLDOWN_SECS - (now - last);
                    if let Ok(Some(e)) = crate::store::kv().get("lastfm.auth_error") {
                        return Err(format!(
                            "Last.fm auth throttled (retry in ~{wait}s): {}",
                            String::from_utf8_lossy(&e)
                        ));
                    }
                    return Err(format!("Last.fm auth throttled (retry in ~{wait}s)"));
                }
            }
        }
        let _ = crate::store::kv().set("lastfm.auth_attempt", now.to_string().into_bytes());
        // Last.fm mobile auth: the login credential is `authToken` =
        // md5(username + md5(password)), NOT a `password` param. Sending
        // `password` is rejected with "Authentication Failed" (error 4) even
        // for correct credentials (verified against the API + pylast).
        let pwd_hash = md5hex(cfg.lastfm_password.as_bytes());
        let auth_token = md5hex(format!("{}{}", cfg.lastfm_user, pwd_hash).as_bytes());
        match lastfm_post(
            cfg,
            "",
            "auth.getMobileSession",
            &[("username", &cfg.lastfm_user), ("authToken", &auth_token)],
        ) {
            Ok(res) => {
                let key = res
                    .pointer("/session/key")
                    .and_then(|k| k.as_str())
                    .ok_or_else(|| format!("could not obtain Last.fm session: {res}"))?
                    .to_string();
                let _ = crate::store::kv().set("lastfm.sk", key.clone().into_bytes());
                let _ = crate::store::kv().delete("lastfm.auth_error");
                Ok(key)
            }
            Err(e) => {
                let _ = crate::store::kv().set("lastfm.auth_error", e.clone().into_bytes());
                Err(e)
            }
        }
    }

    fn nav_call(cfg: &Config, uri: &str) -> Result<Value, String> {
        let user = crate::wasm::scan_user(cfg);
        if user.is_empty() {
            return Err("no Navidrome user available (grant one in User Access) for favorites sync".into());
        }
        let sep = if uri.contains('?') { "&" } else { "?" };
        let json =
            host::subsonicapi::call(&format!("{uri}{sep}u={user}")).map_err(|e| e.to_string())?;
        serde_json::from_str(&json).map_err(|e| e.to_string())
    }

    fn nav_starred(cfg: &Config) -> Vec<StarredSong> {
        nav_call(cfg, "getStarred2")
            .map(|v| parse_starred(&v.to_string()))
            .unwrap_or_default()
    }

    fn nav_search(cfg: &Config, query: &str) -> Vec<StarredSong> {
        let uri = format!("search3?query={}&songCount=20", urlencode(query));
        match nav_call(cfg, &uri) {
            Ok(v) => {
                let songs = v
                    .pointer("/subsonic-response/searchResult3/song")
                    .and_then(|s| s.as_array())
                    .cloned()
                    .unwrap_or_default();
                songs
                    .into_iter()
                    .filter_map(|s| {
                        let id = s.get("id")?.as_str()?.to_string();
                        Some(StarredSong {
                            id,
                            title: s
                                .get("title")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string(),
                            artist: s
                                .get("artist")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string(),
                            album: s
                                .get("album")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string(),
                            mbid: s
                                .get("musicBrainzId")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .collect()
            }
            Err(_) => Vec::new(),
        }
    }

    fn nav_star(cfg: &Config, id: &str) -> Result<(), String> {
        nav_call(cfg, &format!("star?id={id}")).map(|_| ())
    }

    fn lastfm_love(cfg: &Config, sk: &str, artist: &str, title: &str) -> Result<(), String> {
        lastfm_post(
            cfg,
            sk,
            "track.love",
            &[("artist", artist), ("track", title)],
        )
        .map(|_| ())
    }

    /// Scrobble a full listen to ListenBrainz (MusicBrainz ecosystem).
    /// POSTs to https://api.listenbrainz.org/1/submit-listens with a JSON
    /// body containing the track metadata and listen timestamp. Best-effort.
    pub(crate) fn listenbrainz_scrobble(
        cfg: &Config,
        artist: &str,
        title: &str,
        album: &str,
        ts: i64,
    ) {
        let token = cfg.musicbrainz_token.trim();
        if token.is_empty() {
            return;
        }
        let body = serde_json::json!({
            "listen_type": "single",
            "payload": [{
                "listened_at": ts,
                "track_metadata": {
                    "artist_name": artist,
                    "track_name": title,
                    "release_name": album,
                }
            }]
        });
        let req = host::http::HTTPRequest {
            method: "POST".into(),
            url: "https://api.listenbrainz.org/1/submit-listens".to_string(),
            headers: HashMap::from([
                ("Authorization".to_string(), format!("Token {token}")),
                ("Content-Type".to_string(), "application/json".to_string()),
            ]),
            no_follow_redirects: false,
            body: body.to_string().into_bytes(),
            timeout_ms: 15_000,
        };
        if !crate::net::circuit_probe(
            "listenbrainz",
            "https://api.listenbrainz.org",
            &HashMap::new(),
            10_000,
        ) {
            crate::wasm::log_warn("ListenBrainz offline (circuit open)");
            return;
        }
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                crate::wasm::log_info("ListenBrainz scrobble OK");
            }
            Ok(Some(resp)) => crate::wasm::log_warn(&format!(
                "ListenBrainz scrobble HTTP {}", resp.status_code
            )),
            _ => crate::wasm::log_warn("ListenBrainz scrobble failed (no response)"),
        }
    }

    /// Submit a track rating to ListenBrainz. The recording_mbid is required.
    /// Rating scale: 0-100 (ListenBrainz uses 0-100, we convert from 0-5).
    pub(crate) fn listenbrainz_rate_track(
        cfg: &Config,
        recording_mbid: &str,
        rating: f64,
    ) {
        let token = cfg.musicbrainz_token.trim();
        if token.is_empty() || recording_mbid.is_empty() {
            return;
        }
        // Convert 0-5 star rating to 0-100 ListenBrainz scale
        let lb_rating = (rating / 5.0 * 100.0).round() as i32;
        let body = serde_json::json!({
            "rating": {
                "rating_type": "track",
                "rating_value": lb_rating,
                "recording_mbid": recording_mbid,
            }
        });
        listenbrainz_post(cfg, "https://api.listenbrainz.org/1/rating", body);
    }

    /// Submit an album (release group) rating to ListenBrainz.
    pub(crate) fn listenbrainz_rate_album(
        cfg: &Config,
        release_group_mbid: &str,
        rating: f64,
    ) {
        let token = cfg.musicbrainz_token.trim();
        if token.is_empty() || release_group_mbid.is_empty() {
            return;
        }
        let lb_rating = (rating / 5.0 * 100.0).round() as i32;
        let body = serde_json::json!({
            "rating": {
                "rating_type": "release-group",
                "rating_value": lb_rating,
                "release_group_mbid": release_group_mbid,
            }
        });
        listenbrainz_post(cfg, "https://api.listenbrainz.org/1/rating", body);
    }

    /// Submit an artist rating to ListenBrainz.
    pub(crate) fn listenbrainz_rate_artist(
        cfg: &Config,
        artist_mbid: &str,
        rating: f64,
    ) {
        let token = cfg.musicbrainz_token.trim();
        if token.is_empty() || artist_mbid.is_empty() {
            return;
        }
        let lb_rating = (rating / 5.0 * 100.0).round() as i32;
        let body = serde_json::json!({
            "rating": {
                "rating_type": "artist",
                "rating_value": lb_rating,
                "artist_mbid": artist_mbid,
            }
        });
        listenbrainz_post(cfg, "https://api.listenbrainz.org/1/rating", body);
    }

    fn listenbrainz_post(cfg: &Config, url: &str, body: serde_json::Value) {
        let token = cfg.musicbrainz_token.trim();
        if token.is_empty() {
            return;
        }
        if !crate::net::circuit_probe(
            "listenbrainz",
            "https://api.listenbrainz.org",
            &HashMap::new(),
            10_000,
        ) {
            return;
        }
        let req = host::http::HTTPRequest {
            method: "POST".into(),
            url: url.to_string(),
            headers: HashMap::from([
                ("Authorization".to_string(), format!("Token {token}")),
                ("Content-Type".to_string(), "application/json".to_string()),
            ]),
            no_follow_redirects: false,
            body: body.to_string().into_bytes(),
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                crate::wasm::log_info(&format!("ListenBrainz rating OK: {url}"));
            }
            Ok(Some(_resp)) => crate::net::circuit_mark_failed("listenbrainz"),
            _ => {
                crate::net::circuit_mark_failed("listenbrainz");
            }
        }
    }

    /// Scrobble a full listen to Last.fm so its playcount grows too. Best-effort
    /// (fails silently on auth/config problems - session() logs the issue).
    /// Callers should only invoke this when the user opted into lastfmScrobble,
    /// since scrobbling alongside Navidrome's own scrobbler double-counts plays.
    pub(crate) fn scrobble(cfg: &Config, artist: &str, title: &str, album: &str, ts: i64) {
        let Ok(sk) = session(cfg) else {
            return;
        };
        let params: Vec<(String, String)> = vec![
            ("artist".to_string(), artist.to_string()),
            ("track".to_string(), title.to_string()),
            ("album".to_string(), album.to_string()),
            ("timestamp".to_string(), ts.to_string()),
        ];
        let p: Vec<(&str, &str)> = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        if let Err(e) = lastfm_post(cfg, &sk, "track.scrobble", &p) {
            crate::wasm::log_warn(&format!("Last.fm scrobble failed: {e}"));
        }
    }

    /// Last.fm playcount for a track (used to seed the star-tally baseline).
    pub(crate) fn playcount(cfg: &Config, artist: &str, title: &str) -> i64 {
        let res = lastfm_get(
            cfg,
            "track.getInfo",
            &[
                ("artist", artist),
                ("track", title),
                ("username", &cfg.lastfm_user),
                ("autocorrect", "0"),
            ],
        );
        res.and_then(|v| {
            v.pointer("/track/userplaycount")
                .and_then(|p| p.as_str())
                .and_then(|s| s.parse::<i64>().ok())
        })
        .unwrap_or(0)
    }

    /// Whether the user has "loved" this track on Last.fm (from track.getInfo's
    /// `userloved` flag). Used to seed the initial star rating (>= 3).
    pub(crate) fn is_loved(cfg: &Config, artist: &str, title: &str) -> bool {
        let res = lastfm_get(
            cfg,
            "track.getInfo",
            &[
                ("artist", artist),
                ("track", title),
                ("username", &cfg.lastfm_user),
                ("autocorrect", "0"),
            ],
        );
        res.map(|v| v.pointer("/track/userloved").and_then(|p| p.as_str()).map(|s| s == "1"))
            .unwrap_or(Some(false))
            .unwrap_or(false)
    }

    fn lastfm_loved(cfg: &Config) -> Vec<LovedTrack> {
        let mut loved = Vec::new();
        let mut page = 1;
        let limit = 200;
        loop {
            let res = lastfm_get(
                cfg,
                "user.getLovedTracks",
                &[
                    ("user", &cfg.lastfm_user),
                    ("limit", &limit.to_string()),
                    ("page", &page.to_string()),
                ],
            );
            let Some(res) = res else { break };
            let page_tracks = parse_loved(&res.to_string());
            let total = res
                .pointer("/lovedtracks/@attr/total")
                .and_then(|t| t.as_str())
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            loved.extend(page_tracks);
            if loved.len() >= total || page >= 5 {
                break;
            }
            page += 1;
        }
        loved
    }

    /// Star a Last.fm loved track in Navidrome. Returns true when it was found
    /// and starred, false when it isn't in the library.
    fn star_in_navidrome(cfg: &Config, t: &LovedTrack) -> Result<bool, String> {
        let results = nav_search(cfg, &format!("{} {}", t.artist, t.title));
        for s in results {
            if same_track(&t.title, &t.artist, &t.mbid, &s) {
                nav_star(cfg, &s.id)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Bidirectional favorites sync: Navidrome stars <-> Last.fm loved tracks.
    pub fn sync(cfg: &Config) -> Result<SyncSummary, String> {
        let mut summary = SyncSummary::default();
        if !cfg.favorites_sync_lastfm {
            crate::wasm::log_info("favorites sync: Last.fm disabled (favoritesSyncLastfm off)");
            return Ok(summary);
        }
        if cfg.lastfm_api_key.is_empty() || cfg.lastfm_user.is_empty() {
            return Err("favorites sync needs lastfmApiKey + lastfmUser".into());
        }
        let sk = session(cfg)?;
        let max = cfg.favorites_sync_max.max(1);
        let starred = nav_starred(cfg);
        let loved = lastfm_loved(cfg);
        crate::wasm::log_info(&format!(
            "favorites sync: {} starred in Navidrome, {} loved on Last.fm",
            starred.len(),
            loved.len()
        ));

        // Navidrome -> Last.fm (love what's starred but not loved).
        for song in starred.iter().take(max) {
            if song.title.trim().is_empty() || song.artist.trim().is_empty() {
                continue;
            }
            if loved
                .iter()
                .any(|l| same_track(&l.title, &l.artist, &l.mbid, song))
            {
                continue;
            }
            match lastfm_love(cfg, &sk, &song.artist, &song.title) {
                Ok(_) => {
                    summary.nav_to_lastfm += 1;
                    crate::wasm::log_info(&format!(
                        "Last.fm loved: {} - {}",
                        song.artist, song.title
                    ));
                }
                Err(e) => {
                    summary.errors += 1;
                    crate::wasm::log_warn(&format!(
                        "love failed {} - {}: {e}",
                        song.artist, song.title
                    ));
                }
            }
        }

        // Last.fm -> Navidrome (star what's loved but not starred).
        for t in loved.iter().take(max) {
            if starred
                .iter()
                .any(|s| same_track(&t.title, &t.artist, &t.mbid, s))
            {
                continue;
            }
            match star_in_navidrome(cfg, t) {
                Ok(true) => {
                    summary.lastfm_to_nav += 1;
                    crate::wasm::log_info(&format!(
                        "Navidrome starred: {} - {}",
                        t.artist, t.title
                    ));
                }
                Ok(false) => {
                    crate::wasm::log_info(&format!(
                        "not in library (skipped): {} - {}",
                        t.artist, t.title
                    ));
                }
                Err(e) => {
                    summary.errors += 1;
                    crate::wasm::log_warn(&format!("star failed {} - {}: {e}", t.artist, t.title));
                }
            }
        }
        crate::wasm::log_info(&format!(
            "favorites sync done: {}+Navidrome->Last.fm, {}+Last.fm->Navidrome, {} errors",
            summary.nav_to_lastfm, summary.lastfm_to_nav, summary.errors
        ));
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_getstarred2() {
        let json = r#"{"subsonic-response":{"starred2":{"song":[
            {"id":"s1","title":"Dream On","artist":"Aerosmith","album":"Rock","musicBrainzId":"abc"},
            {"id":"s2","title":"Walk This Way","artist":"Aerosmith","album":"Rock"}
        ]}}}"#;
        let songs = parse_starred(json);
        assert_eq!(songs.len(), 2);
        assert_eq!(songs[0].id, "s1");
        assert_eq!(songs[0].mbid, "abc");
        assert_eq!(songs[1].mbid, "");
    }

    #[test]
    fn parses_loved_tracks() {
        let json = r#"{"lovedtracks":{"track":[
            {"name":"Dream On","artist":{"name":"Aerosmith"},"mbid":"abc"},
            {"name":"Bohemian Rhapsody","artist":{"name":"Queen"}}
        ]}}"#;
        let loved = parse_loved(json);
        assert_eq!(loved.len(), 2);
        assert_eq!(loved[0].mbid, "abc");
        assert_eq!(loved[1].mbid, "");
    }

    #[test]
    fn matching_prefers_mbid_then_text() {
        let s = StarredSong {
            id: "x".into(),
            title: "Dream On".into(),
            artist: "Aerosmith".into(),
            album: String::new(),
            mbid: "abc".into(),
        };
        assert!(same_track("Dream On", "Aerosmith", "abc", &s));
        // Wrong mbid -> not same even if title matches.
        assert!(!same_track("Dream On", "Aerosmith", "zzz", &s));
        // No mbid -> text fallback (case-insensitive).
        let s2 = StarredSong {
            id: "x".into(),
            title: "dream on".into(),
            artist: "aerosmith".into(),
            album: String::new(),
            mbid: String::new(),
        };
        assert!(same_track("Dream On", "Aerosmith", "", &s2));
    }
}

