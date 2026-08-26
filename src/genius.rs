// Genius metadata layer: lyrics, annotations, artist backgrounds, and song
// relationships. Used to enrich the library with lyrical content and artist
// context when other sources don't have it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeniusSong {
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub lyrics_state: String,
    pub url: String,
    pub thumbnail: Option<String>,
}

#[cfg(target_arch = "wasm32")]
pub mod host_genius {
    use crate::config::Config;
    use crate::net;
    use nd_pdk::host;
    use std::collections::HashMap;

    use super::*;

    /// Search Genius for a song by artist + title.
    pub fn search_song(cfg: &Config, artist: &str, title: &str) -> Option<GeniusSong> {
        if cfg.genius_token.is_empty() {
            return None;
        }
        if !net::circuit_probe(
            "genius",
            "https://api.genius.com",
            &HashMap::new(),
            10_000,
        ) {
            return None;
        }
        let query = format!("{} {}", artist, title);
        let url = format!(
            "https://api.genius.com/search?q={}",
            crate::favorites::host_favorites::urlencode(&query)
        );
        let mut headers = HashMap::new();
        headers.insert("Authorization".into(), format!("Bearer {}", cfg.genius_token));
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url,
            headers,
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                net::circuit_clear("genius");
                let val: serde_json::Value = serde_json::from_slice(&resp.body).ok()?;
                let hits = val.pointer("/response/hits")?.as_array()?;
                let hit = hits.first()?;
                let song = hit.get("result")?;
                Some(GeniusSong {
                    id: song.get("id")?.as_u64()?,
                    title: song.get("title")?.as_str()?.to_string(),
                    artist: song.get("primary_artist")?
                        .get("name")?.as_str()?.to_string(),
                    album: song.get("album").and_then(|a| a.get("name"))
                        .and_then(|n| n.as_str()).map(String::from),
                    lyrics_state: song.get("lyrics_state")
                        .and_then(|s| s.as_str()).unwrap_or("unknown").to_string(),
                    url: song.get("url")?.as_str()?.to_string(),
                    thumbnail: song.get("song_art_image_thumbnail_url")
                        .and_then(|t| t.as_str()).map(String::from),
                })
            }
            Ok(Some(_)) | Ok(None) | Err(_) => {
                net::circuit_mark_failed("genius");
                None
            }
        }
    }

    /// Fetch lyrics for a Genius song ID.
    pub fn get_lyrics(cfg: &Config, song_id: u64) -> Option<String> {
        if cfg.genius_token.is_empty() {
            return None;
        }
        if !net::circuit_probe(
            "genius",
            "https://api.genius.com",
            &HashMap::new(),
            10_000,
        ) {
            return None;
        }
        let url = format!("https://api.genius.com/songs/{}", song_id);
        let mut headers = HashMap::new();
        headers.insert("Authorization".into(), format!("Bearer {}", cfg.genius_token));
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url,
            headers,
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                net::circuit_clear("genius");
                let val: serde_json::Value = serde_json::from_slice(&resp.body).ok()?;
                let song = val.pointer("/response/song")?;
                let lyrics = song.get("lyrics")?.get("plain")?.as_str()?;
                if lyrics.is_empty() || lyrics == "Lyrics not yet available" {
                    return None;
                }
                Some(lyrics.to_string())
            }
            _ => {
                net::circuit_mark_failed("genius");
                None
            }
        }
    }
}
