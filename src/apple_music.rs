// Apple Music / iTunes metadata layer: album artwork, artist images, artist
// biographies, and similar artists. Uses the free iTunes Search/Lookup APIs
// (no key required) plus Apple Music web scraping for richer data.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppleMusicArtist {
    pub artist_id: i64,
    pub name: String,
    pub image_url: String,
    pub biography: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppleMusicAlbum {
    pub artwork_url: String,
    pub collection_url: String,
    pub description: String,
}

#[cfg(target_arch = "wasm32")]
pub mod host_apple_music {
    use crate::config::Config;
    use crate::net;
    use nd_pdk::host;
    use std::collections::HashMap;

    const ITUNES_SEARCH_URL: &str = "https://itunes.apple.com/search";
    const ITUNES_LOOKUP_URL: &str = "https://itunes.apple.com/lookup";
    const APPLE_MUSIC_BASE: &str = "https://music.apple.com";
    const NEGATIVE_CACHE_TTL: i64 = 7200; // 2 hours

    fn user_agent() -> String {
        "nd-organizer/0.2.0 (https://github.com/Lunatixz/nd-organizer)".into()
    }

    /// Parse comma-separated countries string into a Vec.
    pub fn parse_countries(raw: &str) -> Vec<String> {
        if raw.trim().is_empty() {
            return vec!["us".into()];
        }
        raw.split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    }

    /// Auto-detect country from Navidrome locale env vars.
    pub fn detect_country() -> String {
        if let Ok(locale) = std::env::var("ND_LOCALE") {
            if let Some(country) = locale.split('_').last() {
                return country.to_lowercase();
            }
        }
        if let Ok(lang) = std::env::var("ND_DEFAULT_LANGUAGE") {
            if let Some(country) = lang.split('-').last() {
                return country.to_lowercase();
            }
        }
        "us".into()
    }

    /// Resolve an artist name to an iTunes artist ID. Cached permanently;
    /// negative results cached for 2 hours.
    pub fn resolve_artist_id(artist: &str, countries: &[String]) -> Option<i64> {
        let normalized = artist.trim().to_lowercase();
        if normalized.is_empty() {
            return None;
        }
        let cache_key = format!("am:artist:{normalized}");
        if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
            if let Ok(id) = serde_json::from_slice::<i64>(&v) {
                return if id == 0 { None } else { Some(id) };
            }
        }
        let country = countries.first().map(|s| s.as_str()).unwrap_or("us");
        let url = format!(
            "{}?term={}&entity=musicArtist&limit=5&country={}",
            ITUNES_SEARCH_URL,
            crate::favorites::host_favorites::urlencode(&normalized),
            country
        );
        let mut headers = HashMap::new();
        headers.insert("User-Agent".into(), user_agent());
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url,
            headers,
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 10_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                let val: serde_json::Value = serde_json::from_slice(&resp.body).ok()?;
                let results = val.get("results")?.as_array()?;
                let best = results.iter().find(|r| {
                    r.get("wrapperType").and_then(|w| w.as_str()) == Some("artist")
                })?;
                let id = best.get("artistId")?.as_i64()?;
                let _ = crate::store::kv().set_with_ttl(
                    &cache_key,
                    serde_json::to_vec(&id).unwrap_or_default(),
                    30 * 24 * 3600,
                );
                Some(id)
            }
            _ => {
                let _ = crate::store::kv().set_with_ttl(
                    &cache_key,
                    serde_json::to_vec(&0i64).unwrap_or_default(),
                    NEGATIVE_CACHE_TTL,
                );
                None
            }
        }
    }

    /// Fetch album artwork via iTunes Lookup API. Returns high-res URL
    /// (rewritten from 100x100 to 1500x1500).
    pub fn fetch_album_artwork(
        cfg: &Config,
        artist: &str,
        album: &str,
        countries: &[String],
    ) -> Option<Vec<u8>> {
        let artist_id = resolve_artist_id(artist, countries)?;
        let cache_key = format!("am:art:{}:{}", artist_id, album.to_lowercase());
        if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
            if !v.is_empty() {
                return Some(v);
            }
        }
        if !net::circuit_probe("applemusic", "https://itunes.apple.com", &HashMap::new(), 10_000) {
            return None;
        }
        if !net::throttle("applemusic", 1000) {
            return None;
        }
        let url = format!(
            "{}?id={}&entity=album&limit=200",
            ITUNES_LOOKUP_URL, artist_id
        );
        let mut headers = HashMap::new();
        headers.insert("User-Agent".into(), user_agent());
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url,
            headers,
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 10_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                net::circuit_clear("applemusic");
                let val: serde_json::Value = serde_json::from_slice(&resp.body).ok()?;
                let results = val.get("results")?.as_array()?;
                let album_lower = album.trim().to_lowercase();
                let best = results.iter().find(|r| {
                    r.get("wrapperType").and_then(|w| w.as_str()) == Some("collection")
                        && r.get("collectionName")
                            .and_then(|n| n.as_str())
                            .map(|n| n.to_lowercase() == album_lower)
                            .unwrap_or(false)
                })?;
                let artwork_url = best.get("artworkUrl100")?.as_str()?;
                let hi_res = rewrite_image_size(artwork_url, 1500);
                let img_req = host::http::HTTPRequest {
                    method: "GET".into(),
                    url: hi_res,
                    headers: HashMap::from([("User-Agent".into(), user_agent())]),
                    no_follow_redirects: false,
                    body: vec![],
                    timeout_ms: 15_000,
                };
                match host::http::send(img_req) {
                    Ok(Some(img_resp)) if img_resp.status_code == 200 && !img_resp.body.is_empty() => {
                        let bytes = img_resp.body;
                        let ttl = cfg.apple_music_cache_ttl as i64 * 24 * 3600;
                        let _ = crate::store::kv().set_with_ttl(&cache_key, bytes.clone(), ttl);
                        Some(bytes)
                    }
                    _ => {
                        let _ = crate::store::kv().set_with_ttl(&cache_key, Vec::new(), NEGATIVE_CACHE_TTL);
                        None
                    }
                }
            }
            _ => {
                net::circuit_mark_failed("applemusic");
                None
            }
        }
    }

    /// Rewrite an image URL to a different size (e.g. 100x100 → 1500x1500).
    fn rewrite_image_size(url: &str, size: usize) -> String {
        // Find pattern like /100x100bb. and replace with /1500x1500bb.
        if let Some(start) = url.find('/') {
            if let Some(end) = url[start..].find('.') {
                let segment = &url[start..start + end + 1];
                if segment.contains('x') && segment.chars().any(|c| c.is_ascii_digit()) {
                    let mut result = String::with_capacity(url.len() + 20);
                    result.push_str(&url[..start]);
                    result.push_str(&format!("/{size}x{size}bb"));
                    result.push_str(&url[start + end..]);
                    return result;
                }
            }
        }
        url.to_string()
    }

    /// Fetch artist image from Apple Music web page (JSON-LD or OpenGraph).
    pub fn fetch_artist_image(
        cfg: &Config,
        artist: &str,
        countries: &[String],
    ) -> Option<String> {
        if !cfg.apple_music_artist_images {
            return None;
        }
        let artist_id = resolve_artist_id(artist, countries)?;
        let cache_key = format!("am:img:{artist_id}");
        if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
            if let Ok(s) = serde_json::from_slice::<String>(&v) {
                return if s.is_empty() { None } else { Some(s) };
            }
        }
        if !net::circuit_probe("applemusic", "https://music.apple.com", &HashMap::new(), 10_000) {
            return None;
        }
        if !net::throttle("applemusic", 1000) {
            return None;
        }
        let country = countries.first().map(|s| s.as_str()).unwrap_or("us");
        let page_url = format!("{}/{}/artist/-/{}", APPLE_MUSIC_BASE, country, artist_id);
        let mut headers = HashMap::new();
        headers.insert("User-Agent".into(), user_agent());
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url: page_url,
            headers,
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 10_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                let html = String::from_utf8_lossy(&resp.body);
                let image = extract_image_from_html(&html);
                let ttl = cfg.apple_music_cache_ttl as i64 * 24 * 3600;
                let _ = crate::store::kv().set_with_ttl(
                    &cache_key,
                    serde_json::to_vec(&image.clone().unwrap_or_default()).unwrap_or_default(),
                    ttl,
                );
                image
            }
            _ => {
                let _ = crate::store::kv().set_with_ttl(
                    &cache_key,
                    serde_json::to_vec(&String::new()).unwrap_or_default(),
                    NEGATIVE_CACHE_TTL,
                );
                None
            }
        }
    }

    /// Fetch artist biography from Apple Music web page (JSON-LD).
    pub fn fetch_artist_bio(
        cfg: &Config,
        artist: &str,
        countries: &[String],
    ) -> Option<String> {
        if !cfg.apple_music_artist_bios {
            return None;
        }
        let artist_id = resolve_artist_id(artist, countries)?;
        let cache_key = format!("am:bio:{artist_id}");
        if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
            if let Ok(s) = serde_json::from_slice::<String>(&v) {
                return if s.is_empty() { None } else { Some(s) };
            }
        }
        if !net::circuit_probe("applemusic", "https://music.apple.com", &HashMap::new(), 10_000) {
            return None;
        }
        if !net::throttle("applemusic", 1000) {
            return None;
        }
        let country = countries.first().map(|s| s.as_str()).unwrap_or("us");
        let page_url = format!("{}/{}/artist/-/{}", APPLE_MUSIC_BASE, country, artist_id);
        let mut headers = HashMap::new();
        headers.insert("User-Agent".into(), user_agent());
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url: page_url,
            headers,
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 10_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                let html = String::from_utf8_lossy(&resp.body);
                let bio = extract_bio_from_html(&html);
                let ttl = cfg.apple_music_cache_ttl as i64 * 24 * 3600;
                let _ = crate::store::kv().set_with_ttl(
                    &cache_key,
                    serde_json::to_vec(&bio.clone().unwrap_or_default()).unwrap_or_default(),
                    ttl,
                );
                bio
            }
            _ => {
                let _ = crate::store::kv().set_with_ttl(
                    &cache_key,
                    serde_json::to_vec(&String::new()).unwrap_or_default(),
                    NEGATIVE_CACHE_TTL,
                );
                None
            }
        }
    }

    /// Extract artist image URL from HTML (JSON-LD or OpenGraph).
    fn extract_image_from_html(html: &str) -> Option<String> {
        // Try JSON-LD first
        if let Some(start) = html.find("application/ld+json") {
            let chunk = &html[start..];
            if let Some(ld_start) = chunk.find('{') {
                if let Some(ld_end) = chunk[ld_start..].find("}</script>") {
                    let json_str = &chunk[ld_start..ld_start + ld_end + 1];
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                        if let Some(img) = val.get("image").and_then(|i| i.as_str()) {
                            let url = img.to_string();
                            if !is_placeholder_image(&url) {
                                return Some(url);
                            }
                        }
                    }
                }
            }
        }
        // Fallback to OpenGraph
        if let Some(idx) = html.find(r#"property="og:image" content=""#) {
            let start = idx + 30;
            if let Some(end) = html[start..].find('"') {
                let url = html[start..start + end].to_string();
                if !is_placeholder_image(&url) {
                    return Some(url);
                }
            }
        }
        None
    }

    /// Extract artist biography from HTML (JSON-LD description).
    fn extract_bio_from_html(html: &str) -> Option<String> {
        if let Some(start) = html.find("application/ld+json") {
            let chunk = &html[start..];
            if let Some(ld_start) = chunk.find('{') {
                if let Some(ld_end) = chunk[ld_start..].find("}</script>") {
                    let json_str = &chunk[ld_start..ld_start + ld_end + 1];
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                        if let Some(desc) = val.get("description").and_then(|d| d.as_str()) {
                            let text = normalize_text(desc);
                            if !text.is_empty() && !is_placeholder_bio(&text) {
                                return Some(text);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    fn is_placeholder_image(url: &str) -> bool {
        url.contains("apple-music.png") || url.contains("placeholder")
    }

    fn is_placeholder_bio(text: &str) -> bool {
        text.to_lowercase().contains("apple music")
            && text.to_lowercase().starts_with("listen to")
    }

    fn normalize_text(s: &str) -> String {
        s.replace("\r\n", "\n")
            .replace('\r', "\n")
            .lines()
            .map(|line| {
                line.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }
}
