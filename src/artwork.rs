// Album artwork via Cover Art Archive, Apple Music, and TheAudioDB. Fetches
// front/back/cd/booklet art (cached 7 days, throttled ~1 req/s), embeds it
// into audio tags and/or writes a cover.jpg sidecar.

use std::collections::HashMap;
use std::path::Path;

use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use nd_pdk::host;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtKind {
    Front,
    Back,
    Cd,
    Booklet,
}

impl ArtKind {
    fn slug(self) -> &'static str {
        match self {
            ArtKind::Front => "front",
            ArtKind::Back => "back",
            ArtKind::Cd => "cd",
            ArtKind::Booklet => "booklet",
        }
    }
    fn pic_type(self) -> PictureType {
        match self {
            ArtKind::Front => PictureType::CoverFront,
            ArtKind::Back => PictureType::CoverBack,
            ArtKind::Cd => PictureType::Media,
            ArtKind::Booklet => PictureType::Leaflet,
        }
    }
}

fn sniff_mime(bytes: &[u8]) -> MimeType {
    if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        MimeType::Png
    } else {
        MimeType::Jpeg
    }
}

/// Fetch artwork bytes for a release MBID + kind. Cached 7 days; None when the
/// archive has none (or we're throttled).
pub fn fetch(release_mbid: &str, kind: ArtKind) -> Option<Vec<u8>> {
    let mbid = release_mbid.trim();
    if mbid.is_empty() {
        return None;
    }
    let slug = kind.slug();
    let cache_key = format!("art:{mbid}:{slug}");
    if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    if !crate::net::circuit_probe(
        "coverartarchive",
        "https://coverartarchive.org",
        &HashMap::new(),
        15_000,
    ) {
        return None; // offline - fail fast (auto-recovers via probe)
    }
    if !crate::net::throttle("coverartarchive", 1000) {
        return None;
    }
    let req = host::http::HTTPRequest {
        method: "GET".into(),
        url: format!("https://coverartarchive.org/release/{mbid}/{slug}"),
        headers: HashMap::from([(
            "User-Agent".into(),
            "nd-organizer/0.1 (https://github.com/lunatixz/nd-organizer)".into(),
        )]),
        no_follow_redirects: false,
        body: vec![],
        timeout_ms: 15_000,
    };
    match host::http::send(req) {
        Ok(Some(resp)) if resp.status_code == 200 && !resp.body.is_empty() => {
            crate::net::circuit_clear("coverartarchive");
            let bytes = resp.body;
            let _ = crate::store::kv().set_with_ttl(&cache_key, bytes.clone(), 7 * 24 * 3600);
            Some(bytes)
        }
        Ok(Some(resp)) if resp.status_code == 404 => None, // no such image - not an outage
        Ok(Some(_)) | Ok(None) | Err(_) => {
            crate::net::circuit_mark_failed("coverartarchive");
            None
        }
    }
}

/// True when the file already has embedded artwork.
pub fn has_embedded(path: &Path) -> bool {
    lofty::read_from_path(path)
        .ok()
        .and_then(|t| t.primary_tag().map(|t| !t.pictures().is_empty()))
        .unwrap_or(false)
}

/// Fetch artwork with automatic fallback chain. Tries the selected source first,
/// then other configured sources in order of robustness. Returns (bytes, source_name).
pub fn fetch_with_fallback(
    cfg: &crate::config::Config,
    mbid: Option<&str>,
    artist: &str,
    album: &str,
) -> Option<(Vec<u8>, String)> {
    let countries = crate::apple_music::host_apple_music::parse_countries(&cfg.apple_music_countries);
    let sources = match cfg.artwork_source.as_str() {
        "coverartarchive" => vec!["coverartarchive", "applemusic", "theaudiodb"],
        "applemusic" => vec!["applemusic", "coverartarchive", "theaudiodb"],
        "theaudiodb" => vec!["theaudiodb", "coverartarchive", "applemusic"],
        "embedded" => vec![],
        _ => vec!["coverartarchive", "applemusic", "theaudiodb"],
    };
    for source in sources {
        match source {
            "coverartarchive" => {
                if let Some(m) = mbid {
                    if let Some(bytes) = fetch(m, ArtKind::Front) {
                        return Some((bytes, "coverartarchive".into()));
                    }
                }
            }
            "applemusic" => {
                if cfg.apple_music_album_art {
                    if let Some(bytes) = crate::apple_music::host_apple_music::fetch_album_artwork(
                        cfg, artist, album, &countries,
                    ) {
                        return Some((bytes, "applemusic".into()));
                    }
                }
            }
            "theaudiodb" => {
                if !cfg.theaudiodb_key.is_empty() {
                    if let Some(bytes) = crate::theaudiodb::host_theaudiodb::fetch_album_artwork(
                        cfg, artist, album,
                    ) {
                        return Some((bytes, "theaudiodb".into()));
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Embed artwork into the file's tags (replaces the cover picture).
pub fn embed(path: &Path, bytes: Vec<u8>, kind: ArtKind) -> Result<(), String> {
    let mime = sniff_mime(&bytes);
    let mut tagged = lofty::read_from_path(path).map_err(|e| e.to_string())?;
    let mut tag = tagged.primary_tag().ok_or("no tag block")?.to_owned();
    let pic = Picture::new_unchecked(kind.pic_type(), Some(mime), None, bytes);
    if tag.pictures().is_empty() {
        tag.push_picture(pic);
    } else {
        tag.set_picture(0, pic);
    }
    let _ = tagged.insert_tag(tag);
crate::tags::save_tagged_atomic(&tagged, path)
}

/// Write a cover.jpg sidecar into the album folder (skip if it already exists).
pub fn write_sidecar(dir: &Path, bytes: Vec<u8>) -> Result<(), String> {
    let path = dir.join("cover.jpg");
    if path.exists() {
        return Ok(());
    }
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
crate::tags::atomic_write(&path, &bytes)
}
