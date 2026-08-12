// Reading embedded tags with lofty. Phase 1 is read-only; writing is added in
// Phase 2 once identity verification gates every write.

use std::path::Path;

use lofty::prelude::*;
use lofty::tag::Tag;
use serde::{Deserialize, Serialize};

/// Recording source of a track. A live or bootleg recording is a distinct
/// entity from the studio release and must never be merged/overwritten by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Recording {
    #[default]
    Studio,
    Live,
    Bootleg,
    /// Album-level: tracks disagree on recording source.
    Mixed,
}

impl Recording {
    pub fn as_str(&self) -> &'static str {
        match self {
            Recording::Studio => "",
            Recording::Live => "Live",
            Recording::Bootleg => "Bootleg",
            Recording::Mixed => "Mixed",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
// Some fields are only consumed in Phase 2 (verification/tag-writing).
#[allow(dead_code)]
pub struct TrackTags {
    pub title: String,
    pub artist: String,
    pub album_artist: String,
    pub album: String,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub genre: String,
    pub recording: Recording,
    pub isrc: String,
    pub mbid_recording: String,
    pub mbid_album: String,
    pub mbid_artist: String,
}

/// Detect the recording source from embedded metadata. Bootleg wins over Live.
/// Markers: genre containing "live"/"bootleg", title/album containing
/// "(Live ...)", "[Live", "Live at", etc., and custom LIVE/BOOTLEG tags.
fn detect_recording(tag: &Tag, title: &str, album: &str, genre: &str) -> Recording {
    let g = genre.to_ascii_lowercase();
    let t = title.to_ascii_lowercase();
    let a = album.to_ascii_lowercase();
    let boot = |s: &str| s.contains("bootleg");
    if boot(&g) || boot(&t) || boot(&a) {
        return Recording::Bootleg;
    }
    let live = |s: &str| {
        s.contains("(live") || s.contains("[live") || s.contains("live at") || s.contains(" live ")
    };
    if live(&t) || live(&a) {
        return Recording::Live;
    }

    // Custom LIVE / BOOTLEG tags (ID3 TXXX, Vorbis, MP4 iTunes).
    let mut live_tag = false;
    let mut bootleg_tag = false;
    for item in tag.items() {
        let key = match item.key() {
            ItemKey::Unknown(s) => s.to_ascii_lowercase(),
            _ => String::new(),
        };
        if !key.contains("live") && !key.contains("bootleg") {
            continue;
        }
        let val = item
            .value()
            .text()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let truthy = val.is_empty()
            || matches!(
                val.as_str(),
                "1" | "true" | "yes" | "y" | "live" | "bootleg"
            );
        if truthy {
            if key.contains("bootleg") {
                bootleg_tag = true;
            } else {
                live_tag = true;
            }
        }
    }
    if bootleg_tag {
        Recording::Bootleg
    } else if live_tag {
        Recording::Live
    } else {
        Recording::Studio
    }
}

/// Read tags from an audio file. Returns None when the file has no readable
/// tag block (the file itself may still be fine).
pub fn read_tags(path: &Path) -> Option<TrackTags> {
    let tagged_file = lofty::read_from_path(path).ok()?;
    let tag = tagged_file.primary_tag()?;

    let mbid = |key: ItemKey| tag.get_string(&key).unwrap_or("").to_string();
    let text = |v: Option<std::borrow::Cow<'_, str>>| v.map(|c| c.into_owned()).unwrap_or_default();

    let title = text(tag.title());
    let album = text(tag.album());
    let genre = text(tag.genre());
    let recording = detect_recording(tag, &title, &album, &genre);

    Some(TrackTags {
        title,
        artist: text(tag.artist()),
        album_artist: tag
            .get_string(&ItemKey::AlbumArtist)
            .unwrap_or("")
            .to_string(),
        album,
        year: tag.year(),
        track: tag.track(),
        disc: tag.disk(),
        genre,
        recording,
        isrc: mbid(ItemKey::Isrc),
        mbid_recording: mbid(ItemKey::MusicBrainzRecordingId),
        mbid_album: mbid(ItemKey::MusicBrainzReleaseId),
        mbid_artist: mbid(ItemKey::MusicBrainzArtistId),
    })
}
