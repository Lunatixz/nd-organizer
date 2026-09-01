// TheAudioDB metadata layer: fanart, artist bios, album descriptions,
// album artwork, and genre tags.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TheAudioDbArtist {
    pub id: String,
    pub name: String,
    pub biography: String,
    pub banner: String,
    pub fanart: String,
    pub thumb: String,
    pub genre: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TheAudioDbAlbum {
    pub id: String,
    pub name: String,
    pub description: String,
    pub str_artwork: String,
    pub genre: String,
}

#[cfg(target_arch = "wasm32")]
pub mod host_theaudiodb {
    use crate::config::Config;
    use crate::net;
    use nd_pdk::host;
    use std::collections::HashMap;

    use super::*;

    /// Search for an artist by name. Gated by `theaudiodbFanart` config.
    pub fn search_artist(cfg: &Config, name: &str) -> Option<TheAudioDbArtist> {
        if cfg.theaudiodb_key.is_empty() || !cfg.theaudiodb_fanart {
            return None;
        }
        if !net::circuit_probe(
            "theaudiodb",
            "https://theaudiodb.com",
            &HashMap::new(),
            10_000,
        ) {
            return None;
        }
        let url = format!(
            "https://theaudiodb.com/api/v1/json/{}/search.php?s={}",
            cfg.theaudiodb_key,
            crate::favorites::host_favorites::urlencode(name)
        );
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url,
            headers: HashMap::new(),
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                net::circuit_clear("theaudiodb");
                let val: serde_json::Value = serde_json::from_slice(&resp.body).ok()?;
                let artists = val.get("artists")?.as_array()?;
                let a = artists.first()?;
                Some(TheAudioDbArtist {
                    id: a.get("idArtist")?.as_str()?.to_string(),
                    name: a.get("strArtist")?.as_str()?.to_string(),
                    biography: a.get("strBiographyEN")?.as_str()?.to_string(),
                    banner: a.get("strArtistBanner")?.as_str()?.to_string(),
                    fanart: a.get("strArtistFanart")?.as_str()?.to_string(),
                    thumb: a.get("strArtistThumb")?.as_str()?.to_string(),
                    genre: a.get("strGenre")?.as_str()?.to_string(),
                })
            }
            Ok(Some(_)) | Ok(None) | Err(_) => {
                net::circuit_mark_failed("theaudiodb");
                None
            }
        }
    }

    /// Search for an album by artist + album name.
    pub fn search_album(cfg: &Config, artist: &str, album: &str) -> Option<TheAudioDbAlbum> {
        if cfg.theaudiodb_key.is_empty() {
            return None;
        }
        if !net::circuit_probe(
            "theaudiodb",
            "https://theaudiodb.com",
            &HashMap::new(),
            10_000,
        ) {
            return None;
        }
        let url = format!(
            "https://theaudiodb.com/api/v1/json/{}/search.php?t={}",
            cfg.theaudiodb_key,
            crate::favorites::host_favorites::urlencode(&format!("{} {}", artist, album))
        );
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url,
            headers: HashMap::new(),
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                net::circuit_clear("theaudiodb");
                let val: serde_json::Value = serde_json::from_slice(&resp.body).ok()?;
                let albums = val.get("album")?.as_array()?;
                let a = albums.first()?;
                Some(TheAudioDbAlbum {
                    id: a.get("idAlbum")?.as_str()?.to_string(),
                    name: a.get("strAlbum")?.as_str()?.to_string(),
                    description: a.get("strDescriptionEN")?.as_str()?.to_string(),
                    str_artwork: a.get("strAlbumThumb")?.as_str()?.to_string(),
                    genre: a.get("strGenre")?.as_str()?.to_string(),
                })
            }
            Ok(Some(_)) | Ok(None) | Err(_) => {
                net::circuit_mark_failed("theaudiodb");
                None
            }
        }
    }

    /// Download album artwork bytes from TheAudioDB.
    pub fn fetch_album_artwork(cfg: &Config, artist: &str, album: &str) -> Option<Vec<u8>> {
        if cfg.theaudiodb_key.is_empty() {
            return None;
        }
        let album_data = search_album(cfg, artist, album)?;
        let artwork_url = album_data.str_artwork;
        if artwork_url.is_empty() {
            return None;
        }
        let cache_key = format!("tdb:art:{}", album_data.id);
        if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
            if !v.is_empty() {
                return Some(v);
            }
        }
        if !net::circuit_probe("theaudiodb", "https://theaudiodb.com", &HashMap::new(), 10_000) {
            return None;
        }
        if !net::throttle("theaudiodb", 1000) {
            return None;
        }
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url: artwork_url,
            headers: HashMap::new(),
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 && !resp.body.is_empty() => {
                net::circuit_clear("theaudiodb");
                let bytes = resp.body;
                let _ = crate::store::kv().set_with_ttl(&cache_key, bytes.clone(), 7 * 24 * 3600);
                Some(bytes)
            }
            _ => {
                net::circuit_mark_failed("theaudiodb");
                None
            }
        }
    }

    /// Fetch genre tags for an album from TheAudioDB.
    pub fn fetch_genres(cfg: &Config, artist: &str, album: &str) -> Option<Vec<String>> {
        if cfg.theaudiodb_key.is_empty() {
            return None;
        }
        let album_data = search_album(cfg, artist, album)?;
        let genre = album_data.genre;
        if genre.is_empty() {
            return None;
        }
        Some(genre.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
    }
}
