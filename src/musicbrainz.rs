// MusicBrainz metadata layer: album release lookups used for classification
// (release type -> Soundtrack/Compilation/Single) and artwork MBIDs. The
// optional MusicBrainz token raises the rate limit from ~1 to ~50 req/s.

use std::collections::HashMap;

use nd_pdk::host;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MbRelease {
    pub release_mbid: String,
    pub release_group_mbid: String,
    /// e.g. "Album", "Single", "EP", "Soundtrack".
    pub primary_type: String,
    /// e.g. ["Compilation"], ["Live"].
    pub secondary_types: Vec<String>,
    pub title: String,
    pub date: Option<String>,
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

/// Look up a release by artist + album title. Returns the best (title-exact,
/// release-group-bearing) match, cached 7 days. The `token` (if any) is sent as
/// a bearer token to raise MusicBrainz's rate limit.
pub fn lookup(artist: &str, album: &str, token: &str) -> Option<MbRelease> {
    if artist.trim().is_empty() || album.trim().is_empty() {
        return None;
    }
    let cache_key = format!("mb:{}|{}", artist.to_lowercase(), album.to_lowercase());
    if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
        if let Ok(r) = serde_json::from_slice::<MbRelease>(&v) {
            return Some(r);
        }
    }
    if !crate::net::circuit_probe(
        "musicbrainz",
        "https://musicbrainz.org/ws/2/",
        &HashMap::new(),
        15_000,
    ) {
        return None; // offline - fail fast (auto-recovers via probe)
    }
    if !crate::net::throttle("musicbrainz", if token.trim().is_empty() { 1200 } else { 30 }) {
        return None;
    }
    let query = format!(
        "https://musicbrainz.org/ws/2/release/?query=release:%22{}%22%20AND%20artist:%22{}%22&fmt=json&limit=5",
        urlenc(album),
        urlenc(artist)
    );
    let mut headers = HashMap::from([(
        "User-Agent".to_string(),
        "nd-organizer/0.1 (https://github.com/lunatixz/nd-organizer)".to_string(),
    )]);
    if !token.trim().is_empty() {
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", token.trim()),
        );
    }
    let req = host::http::HTTPRequest {
        method: "GET".into(),
        url: query,
        headers,
        no_follow_redirects: false,
        body: vec![],
        timeout_ms: 15_000,
    };
    let result: Option<MbRelease> = match host::http::send(req) {
        Ok(Some(resp)) if resp.status_code == 200 => {
            crate::net::circuit_clear("musicbrainz");
            let Ok(v) = serde_json::from_slice::<Value>(&resp.body) else {
                return None;
            };
            parse_releases(&v, artist, album)
        }
        Ok(Some(resp)) if resp.status_code == 404 => None, // no match - not an outage
        Ok(Some(_)) | Ok(None) | Err(_) => {
            crate::net::circuit_mark_failed("musicbrainz");
            None
        }
    };
    if let Some(r) = &result {
        let _ = crate::store::kv().set_with_ttl(
            &cache_key,
            serde_json::to_vec(r).unwrap_or_default(),
            7 * 24 * 3600,
        );
    }
    result
}

#[derive(Deserialize)]
struct RawRelease {
    id: String,
    title: String,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    #[serde(rename = "release-group")]
    release_group: Option<RawReleaseGroup>,
}

#[derive(Deserialize)]
struct RawReleaseGroup {
    id: String,
    #[serde(default)]
    #[serde(rename = "primary-type")]
    primary_type: Option<String>,
    #[serde(default)]
    #[serde(rename = "secondary-types")]
    secondary_types: Vec<String>,
}

/// Pick the best release: exact (case-insensitive) title match preferred, then
/// one that carries a release group. Falls back to the first result.
fn parse_releases(v: &Value, _artist: &str, album: &str) -> Option<MbRelease> {
    let releases: Vec<RawRelease> = serde_json::from_value(v.get("releases")?.clone()).ok()?;
    if releases.is_empty() {
        return None;
    }
    let want = album.trim().to_lowercase();
    let ranked = releases.into_iter().filter_map(|r| {
        let title_ok = r.title.to_lowercase() == want;
        let group = r.release_group.as_ref();
        let has_group = group.is_some();
        // Prefer results that look like the artist/album we asked for.
        let score = usize::from(title_ok) * 4 + usize::from(has_group) * 2;
        Some((
            score,
            MbRelease {
                release_mbid: r.id,
                release_group_mbid: group.map(|g| g.id.clone()).unwrap_or_default(),
                primary_type: group
                    .and_then(|g| g.primary_type.clone())
                    .unwrap_or_default(),
                secondary_types: group.map(|g| g.secondary_types.clone()).unwrap_or_default(),
                title: r.title,
                date: r.date,
            },
        ))
    });
    let mut all: Vec<_> = ranked.collect();
    all.sort_by(|a, b| b.0.cmp(&a.0));
    all.first().map(|(_, r)| r.clone())
}
