// TheAudioDB metadata layer: fanart, artist bios, and album descriptions.
//
// Used to enrich the library with visual assets (artist backgrounds, banners)
// and textual metadata (biographies, album descriptions).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TheAudioDbArtist {
    pub id: String,
    pub name: String,
    pub biography: String,
    pub banner: String,
    pub fanart: String,
    pub thumb: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TheAudioDbAlbum {
    pub id: String,
    pub name: String,
    pub description: String,
    pub str_artwork: String,
}

#[cfg(target_arch = "wasm32")]
pub mod host_theaudiodb {
    use crate::config::Config;
    use crate::net;
    use nd_pdk::host;
    use std::collections::HashMap;

    use super::*;

    /// Search for an artist by name.
    pub fn search_artist(cfg: &Config, name: &str) -> Option<TheAudioDbArtist> {
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
                })
            }
            Ok(Some(_)) | Ok(None) | Err(_) => {
                net::circuit_mark_failed("theaudiodb");
                None
            }
        }
    }
}
