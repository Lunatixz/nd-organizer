// Reading embedded tags with lofty. Phase 1 is read-only; writing is added in
// Phase 2 once identity verification gates every write.

use std::path::Path;

use lofty::prelude::*;
use lofty::tag::Tag;
use serde::{Deserialize, Serialize};

/// Crash-safe file replace: write to a temp sibling, fsync, then rename over
/// the target. The target is always the previous or the new file - never a torn
/// write, even if the process crashes mid-save.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let tmp = parent.join(format!(".{name}.ndtmp"));
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(data).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(())
}

/// Save a TaggedFile to `path` atomically: render to a temp sibling, fsync,
/// then rename over the target. Never writes in place.
pub fn save_tagged_atomic<T: lofty::prelude::AudioFile>(
    tagged: &T,
    path: &Path,
) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let tmp = parent.join(format!(".{name}.ndtmp"));
    tagged
        .save_to_path(&tmp, lofty::config::WriteOptions::default())
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.to_string()
        })?;
    if let Ok(f) = std::fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(())
}

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

/// Should a tag item be (re)written? With `overwrite=false` only missing fields
/// are filled (existing values are preserved); `overwrite=true` replaces them.
/// Always skips when the value is unchanged (no rewrite churn).
pub fn should_write(existing: &str, new: &str, overwrite: bool) -> bool {
    if existing == new {
        return false;
    }
    if !overwrite && !existing.is_empty() {
        return false;
    }
    true
}

/// Persist a resolved identity into the file's own tags. Once the file carries
/// an MBID, every later run reads it straight out of the tags (confidence ->
/// Verified), so AcoustID is never re-queried for it - even after the ident
/// cache expires or the file is moved/renamed by the organizer. Idempotent:
/// files that already have the album MBID are left untouched (no rewrite churn).
pub fn write_mbids(
    path: &Path,
    album_mbid: &str,
    recording_mbid: Option<&str>,
    overwrite: bool,
) -> Result<(), String> {
    if album_mbid.is_empty() {
        return Ok(());
    }
    let mut tagged = lofty::read_from_path(path).map_err(|e| e.to_string())?;
    let mut tag = tagged.primary_tag().ok_or("no tag block")?.to_owned();
    let existing_album = tag.get_string(&ItemKey::MusicBrainzReleaseId).unwrap_or("");
    if !should_write(existing_album, album_mbid, overwrite) {
        return Ok(());
    }
    tag.insert_text(ItemKey::MusicBrainzReleaseId, album_mbid.to_string());
    if let Some(rec) = recording_mbid.filter(|r| !r.is_empty()) {
        if should_write(
            tag.get_string(&ItemKey::MusicBrainzRecordingId).unwrap_or(""),
            rec,
            overwrite,
        ) {
            tag.insert_text(ItemKey::MusicBrainzRecordingId, rec.to_string());
        }
    }
    let _ = tagged.insert_tag(tag);
save_tagged_atomic(&tagged, path)
}

/// Write playback metadata into a track's tags (opt-in, `writePlaycount`):
/// `FMPS_PLAYCOUNT` (playcount), `RATING` (0.0-5.0 star value), and `LOVED`
/// (1/0 favorite status). Idempotent + respects the fill-only/overwrite policy.
pub fn write_playback_meta(
    path: &Path,
    stars: Option<f64>,
    playcount: i64,
    loved: Option<bool>,
    overwrite: bool,
) -> Result<(), String> {
    let mut tagged = lofty::read_from_path(path).map_err(|e| e.to_string())?;
    let mut tag = tagged.primary_tag().ok_or("no tag block")?.to_owned();
    let mut changed = false;
    if playcount > 0 {
        let pc = playcount.to_string();
        let existing = tag.get_string(&ItemKey::Unknown("FMPS_PLAYCOUNT".into())).unwrap_or("");
        if should_write(existing, &pc, overwrite) {
            tag.insert_text(ItemKey::Unknown("FMPS_PLAYCOUNT".into()), pc);
            changed = true;
        }
    }
    if let Some(s) = stars {
        let val = format!("{s}");
        let existing = tag.get_string(&ItemKey::Unknown("RATING".into())).unwrap_or("");
        if should_write(existing, &val, overwrite) {
            tag.insert_text(ItemKey::Unknown("RATING".into()), val);
            changed = true;
        }
    }
    if let Some(l) = loved {
        let val = if l { "1" } else { "0" };
        let existing = tag.get_string(&ItemKey::Unknown("LOVED".into())).unwrap_or("");
        if should_write(existing, val, overwrite) {
            tag.insert_text(ItemKey::Unknown("LOVED".into()), val.to_string());
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let _ = tagged.insert_tag(tag);
save_tagged_atomic(&tagged, path)
}
