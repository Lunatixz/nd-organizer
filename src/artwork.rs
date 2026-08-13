// Album artwork via the Cover Art Archive (MusicBrainz). Fetches front/back/
// cd/booklet art for a release MBID (cached 7 days, throttled ~1 req/s), embeds
// it into audio tags and/or writes a cover.jpg sidecar.

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
    tagged
        .save_to_path(path, lofty::config::WriteOptions::default())
        .map_err(|e| e.to_string())
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
    std::fs::write(&path, bytes).map_err(|e| e.to_string())
}
