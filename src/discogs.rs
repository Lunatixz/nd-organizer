// Discogs metadata layer: release credits, community ratings, and catalog info.
//
// Used to enrich album.nfo files with detailed credits (performers, producers,
// engineers) and to seed star ratings from community ratings.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscogsRelease {
    pub id: u64,
    pub title: String,
    pub year: Option<u32>,
    pub genres: Vec<String>,
    pub styles: Vec<String>,
    pub community_rating: Option<f64>,
    pub num_ratings: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscogsCredit {
    pub name: String,
    pub role: String,
}

#[cfg(target_arch = "wasm32")]
pub mod host_discogs {
    use crate::config::Config;
    use crate::net;
    use nd_pdk::host;
    use std::collections::HashMap;

    use super::*;

    fn headers(cfg: &Config) -> HashMap<String, String> {
        let mut h = HashMap::new();
        h.insert("User-Agent".into(), "nd-organizer/0.2.0 (https://github.com/Lunatixz/nd-organizer)".into());
        if !cfg.discogs_token.is_empty() {
            h.insert("Authorization".into(), format!("Discogs token={}", cfg.discogs_token));
        }
        h
    }

    /// Search Discogs for a release by artist + album. Returns the best match.
    pub fn search_release(cfg: &Config, artist: &str, album: &str) -> Option<DiscogsRelease> {
        if cfg.discogs_token.is_empty() {
            return None;
        }
        if !net::circuit_probe(
            "discogs",
            "https://api.discogs.com",
            &HashMap::new(),
            10_000,
        ) {
            return None;
        }
        let query = format!("{} {}", artist, album);
        let url = format!(
            "https://api.discogs.com/database/search?q={}&type=release&per_page=5",
            crate::favorites::host_favorites::urlencode(&query)
        );
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url,
            headers: headers(cfg),
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                net::circuit_clear("discogs");
                let val: serde_json::Value = serde_json::from_slice(&resp.body).ok()?;
                let results = val.get("results")?.as_array()?;
                let best = results.first()?;
                Some(DiscogsRelease {
                    id: best.get("id")?.as_u64()?,
                    title: best.get("title")?.as_str()?.to_string(),
                    year: best.get("year").and_then(|y| y.as_u64()).map(|y| y as u32),
                    genres: best.get("genre")
                        .and_then(|g| g.as_array())
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                        .unwrap_or_default(),
                    styles: best.get("style")
                        .and_then(|s| s.as_array())
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                        .unwrap_or_default(),
                    community_rating: best.get("community")
                        .and_then(|c| c.get("rating"))
                        .and_then(|r| r.get("average"))
                        .and_then(|a| a.as_f64()),
                    num_ratings: best.get("community")
                        .and_then(|c| c.get("rating"))
                        .and_then(|r| r.get("num_ratings"))
                        .and_then(|n| n.as_u64())
                        .map(|n| n as u32),
                })
            }
            Ok(Some(_)) | Ok(None) | Err(_) => {
                net::circuit_mark_failed("discogs");
                None
            }
        }
    }

    /// Fetch credits for a Discogs release ID.
    pub fn get_credits(cfg: &Config, release_id: u64) -> Vec<DiscogsCredit> {
        if cfg.discogs_token.is_empty() {
            return Vec::new();
        }
        if !net::circuit_probe(
            "discogs",
            "https://api.discogs.com",
            &HashMap::new(),
            10_000,
        ) {
            return Vec::new();
        }
        let url = format!("https://api.discogs.com/releases/{}", release_id);
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url,
            headers: headers(cfg),
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                net::circuit_clear("discogs");
                let val: serde_json::Value = serde_json::from_slice(&resp.body).ok().unwrap_or_default();
                let mut credits = Vec::new();
                // Extraartists (performers, producers, engineers)
                if let Some(extra) = val.get("extraartists").and_then(|e| e.as_array()) {
                    for a in extra {
                        if let (Some(name), Some(role)) = (
                            a.get("name").and_then(|n| n.as_str()),
                            a.get("role").and_then(|r| r.as_str()),
                        ) {
                            credits.push(DiscogsCredit {
                                name: name.to_string(),
                                role: role.to_string(),
                            });
                        }
                    }
                }
                credits
            }
            _ => {
                net::circuit_mark_failed("discogs");
                Vec::new()
            }
        }
    }
}
