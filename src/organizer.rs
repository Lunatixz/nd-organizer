// Album discovery, classification and rename-plan construction.
//
// All functions operate on a `root` path so unit tests can run against temp
// directories on the host. In the plugin the root is the library mount point
// (e.g. `/libraries/1`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::nfo::{self, NfoAlbum, NfoArtist};
use crate::tags::{read_tags, Recording, TrackTags};
use crate::template::{render_file_name, render_folder_path, SanitizeOptions, TemplateFields};

pub const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "m4a", "aac", "ogg", "oga", "opus", "wav", "wv", "aiff", "aif", "ape", "mpc",
];
const SIDECAR_EXTS: &[&str] = &["lrc", "jpg", "jpeg", "png", "nfo", "cue"];

/// Album-level files that always move with the folder (not tied to a track stem).
const DIR_SIDECARS: &[&str] = &[
    "album.nfo", "artist.nfo",
    "cover.jpg", "cover.png", "cover.jpeg",
    "folder.jpg", "folder.png",
    "front.jpg", "front.png", "back.jpg", "back.png", "cd.jpg", "cd.png",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Bucket {
    Soundtrack,
    Various,
    Singles,
    #[default]
    Normal,
}

#[derive(Debug, Clone)]
pub struct TrackEntry {
    pub name: String,
    pub tags: Option<TrackTags>,
}

/// An album is a directory that contains at least one audio file.
#[derive(Debug, Clone)]
pub struct AlbumDir {
    /// Path relative to the library root ("" for root-level files).
    pub dir: String,
    pub tracks: Vec<TrackEntry>,
}

/// Aggregate album-level fields used by the classifier and templates.
#[derive(Debug, Clone)]
pub struct AlbumInfo {
    pub album: String,
    pub album_artist: String,
    pub year: Option<u32>,
    pub genre: String,
    pub track_count: usize,
    pub distinct_artists: Vec<String>,
    /// Uniform recording source (Live/Bootleg) or Mixed when tracks disagree.
    pub recording: Recording,
}

#[derive(Debug, Clone)]
pub struct FileMove {
    /// Root-relative source path.
    pub from: String,
    /// Root-relative target path.
    pub to: String,
    /// Sidecar file names (within the album dir) to move alongside.
    pub sidecars: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AlbumPlan {
    pub bucket: Bucket,
    pub current_dir: String,
    pub target_dir: String,
    pub moves: Vec<FileMove>,
    /// Album-level files (album.nfo, cover.jpg, ...) to move with the folder.
    pub dir_sidecars: Vec<String>,
    pub keeps: usize,
    /// (path, reason) entries skipped because a target already exists or the
    /// target is unbuildable.
    pub skipped: Vec<(String, String)>,
}

/// Walk the library root and return every directory containing audio files.
/// `skip_hidden` controls whether dot-directories are skipped.
#[cfg(test)]
pub fn discover_albums(root: &Path) -> Vec<AlbumDir> {
    discover_albums_skip(root, true, &[], 0, 0)
}

/// Like `discover_albums`, honoring the user's hidden-files preference and
/// exclusion patterns. `limit` bounds how many albums are collected (0 =
/// unlimited); `max_entries` bounds total directory entries examined before the
/// walk stops (0 = unlimited), keeping passes fast on huge libraries. Tag
/// reading is deliberately NOT done here: it is deferred to per-album planning.
pub fn discover_albums_skip(
    root: &Path,
    skip_hidden: bool,
    excludes: &[String],
    limit: usize,
    max_entries: usize,
) -> Vec<AlbumDir> {
    let mut albums = Vec::new();
    let mut scanned = 0usize;
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        if (limit > 0 && albums.len() >= limit) || (max_entries > 0 && scanned >= max_entries) {
            break;
        }
        if is_excluded(&rel, excludes) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut subdirs = Vec::new();
        let mut tracks: Vec<TrackEntry> = Vec::new();
        for entry in entries.flatten() {
            scanned += 1;
            let name = entry.file_name().to_string_lossy().to_string();
            if skip_hidden && name.starts_with('.') {
                continue;
            }
            // Audio files are recognized by extension (no stat needed).
            if is_audio(&name) {
                tracks.push(TrackEntry { name, tags: None });
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                subdirs.push(entry.path());
            }
        }
        if !tracks.is_empty() {
            albums.push(AlbumDir { dir: rel.clone(), tracks });
        }
        for sub in subdirs {
            let sub_rel = if rel.is_empty() {
                sub.file_name().unwrap().to_string_lossy().to_string()
            } else {
                format!("{}/{}", rel, sub.file_name().unwrap().to_string_lossy())
            };
            stack.push((sub, sub_rel));
        }
    }
    albums.sort_by(|a, b| a.dir.cmp(&b.dir));
    albums
}

/// True when a relative directory path is covered by an exclusion pattern.
/// A pattern is either a directory prefix ("inbox", "Downloads") or a glob of
/// the form "prefix/*"; both exclude the dir and everything beneath it.
fn is_excluded(rel: &str, excludes: &[String]) -> bool {
    excludes.iter().any(|p| {
        let p = p.trim_end_matches('/');
        if let Some(prefix) = p.strip_suffix("/*") {
            rel == prefix || rel.starts_with(&format!("{prefix}/"))
        } else {
            rel == p || rel.starts_with(&format!("{p}/"))
        }
    })
}

fn sanitize_opts(cfg: &Config) -> SanitizeOptions {
    SanitizeOptions {
        illegal_char_replacement: cfg
            .illegal_char_replacement
            .chars()
            .next()
            .unwrap_or('_'),
        max_name_length: cfg.max_name_length.max(1),
    }
}

fn is_audio(name: &str) -> bool {
    name.rsplit_once('.')
        .map(|(_, ext)| AUDIO_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn is_sidecar(name: &str) -> bool {
    name.rsplit_once('.')
        .map(|(_, ext)| SIDECAR_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Aggregate album-level fields from embedded tags only (no fallbacks).
fn album_info_raw(album: &AlbumDir) -> AlbumInfo {
    let mut title = String::new();
    let mut artist = String::new();
    let mut year = None;
    let mut genre = String::new();
    let mut distinct_artists: Vec<String> = Vec::new();
    let mut recordings: Vec<Recording> = Vec::new();
    for t in &album.tracks {
        if let Some(tags) = &t.tags {
            if title.is_empty() {
                title = tags.album.clone();
            }
            if artist.is_empty() {
                artist = tags.album_artist.clone();
            }
            if year.is_none() {
                year = tags.year;
            }
            if genre.is_empty() {
                genre = tags.genre.clone();
            }
            let ta = tags.artist.trim();
            if !ta.is_empty() && !distinct_artists.iter().any(|a| a.eq_ignore_ascii_case(ta)) {
                distinct_artists.push(ta.to_string());
            }
            if !recordings.contains(&tags.recording) {
                recordings.push(tags.recording);
            }
        }
    }
    let recording = match recordings.len() {
        0 => Recording::Studio,
        1 => recordings[0],
        _ => Recording::Mixed,
    };
    AlbumInfo {
        album: title,
        album_artist: artist,
        year,
        genre,
        track_count: album.tracks.len(),
        distinct_artists,
        recording,
    }
}

/// Apply the weakest fallbacks (folder/file names) only for still-empty fields.
fn apply_folder_fallbacks(info: &mut AlbumInfo, album: &AlbumDir) {
    if info.album.is_empty() {
        info.album = folder_name(&album.dir).to_string();
    }
    if info.album_artist.is_empty() && info.distinct_artists.len() == 1 {
        info.album_artist = info.distinct_artists[0].clone();
    }
}

/// Aggregate album-level fields from its tracks.
#[cfg(test)]
pub fn album_info(album: &AlbumDir) -> AlbumInfo {
    let mut info = album_info_raw(album);
    apply_folder_fallbacks(&mut info, album);
    info
}

impl AlbumInfo {
    /// Fill empty fields from an album.nfo. Tags take priority; NFO only fills
    /// gaps (including values APIs can't provide, like styles/moods).
    pub fn merge_album_nfo(&mut self, nfo: &NfoAlbum) {
        if self.album.is_empty() {
            self.album = nfo.title.clone();
        }
        if self.album_artist.is_empty() {
            self.album_artist = nfo.album_artists.first().cloned().unwrap_or_default();
        }
        if self.year.is_none() {
            self.year = nfo.year;
        }
        if self.genre.is_empty() {
            self.genre = first_of(&nfo.genres, &nfo.styles, &nfo.moods);
        }
    }

    /// Fill empty fields from an artist.nfo.
    pub fn merge_artist_nfo(&mut self, artist: &NfoArtist) {
        if self.album_artist.is_empty() {
            self.album_artist = artist.name.clone();
        }
        if self.genre.is_empty() {
            self.genre = first_of(&artist.genres, &artist.styles, &artist.moods);
        }
    }
}

fn first_of(a: &[String], b: &[String], c: &[String]) -> String {
    a.first()
        .or_else(|| b.first())
        .or_else(|| c.first())
        .cloned()
        .unwrap_or_default()
}

/// Build an AlbumInfo merging embedded tags with NFO sidecars (when enabled).
/// Priority: embedded tags > NFO > folder/file names.
pub fn album_info_with_nfo(album: &AlbumDir, cfg: &Config, root: &Path) -> AlbumInfo {
    let mut info = album_info_raw(album);
    if cfg.read_nfo {
        let dir = root.join(&album.dir);
        if let Some(nfo) = nfo::read_album_nfo(&dir) {
            info.merge_album_nfo(&nfo);
        }
        if let Some(artist) = nfo::read_artist_nfo(&dir) {
            info.merge_artist_nfo(&artist);
        }
    }
    apply_folder_fallbacks(&mut info, album);
    info
}

fn folder_name(rel: &str) -> String {
    rel.rsplit('/').next().unwrap_or(rel).to_string()
}

/// Classify an album into a bucket. Local-tag heuristics only; MusicBrainz /
/// Lidarr release-type signals slot in ahead of this in Phase 2.
pub fn classify(info: &AlbumInfo, cfg: &Config) -> Bucket {
    let genre = info.genre.to_ascii_lowercase();
    let album = info.album.to_ascii_lowercase();
    let is_soundtrack = genre.contains("soundtrack")
        || genre.contains("ost")
        || album.contains("soundtrack")
        || album.contains("original motion picture")
        || album.contains("music from")
        || album.contains("music inspired by");
    if is_soundtrack {
        return Bucket::Soundtrack;
    }

    let aa = info.album_artist.trim().to_ascii_lowercase();
    let is_various = aa == "various" || aa == "various artists" || aa == "va"
        || (info.distinct_artists.len() > 1 && aa.is_empty());
    if is_various {
        return Bucket::Various;
    }

    if info.track_count == 1 || info.track_count < cfg.incomplete_album_min_tracks {
        return Bucket::Singles;
    }

    Bucket::Normal
}

fn album_fields(info: &AlbumInfo, first_title: &str) -> TemplateFields {
    let title = if info.album.is_empty() {
        first_title.to_string()
    } else {
        info.album.clone()
    };
    TemplateFields {
        track: None,
        disc: None,
        title,
        artist: info.album_artist.clone(),
        album_artist: info.album_artist.clone(),
        album: info.album.clone(),
        year: info.year,
        genre: info.genre.clone(),
        recording: if matches!(info.recording, Recording::Mixed) {
            String::new()
        } else {
            info.recording.as_str().to_string()
        },
        mbid: String::new(),
    }
}

/// Compute the target album directory (root-relative) for a bucket.
///
/// Layout:
///   Soundtracks -> Various Artist/Sound Tracks/{album} ({year})
///   Various     -> Various Artist/{album} ({year})
///   Singles     -> {albumArtist}/{singlesFolder}/{title}  (when
///                  `singlesUnderArtist` and the artist is known), else
///                  Various Artist/Singles/{albumArtist} - {title}
///   Normal      -> {folderSchema}
///
/// Live/bootleg albums get a " (Live)"/" (Bootleg)" suffix so they never
/// collide with the studio release.
pub fn target_album_dir(bucket: Bucket, info: &AlbumInfo, cfg: &Config, first_title: &str) -> String {
    let fields = album_fields(info, first_title);
    let opts = sanitize_opts(cfg);
    let rendered = match bucket {
        Bucket::Soundtrack => {
            let sub = render_folder_path(&format!("{}/{{album}} ({{year}})", cfg.soundtrack_folder), &fields, &opts);
            if cfg.nest_buckets_under_various {
                format!("{}/{}", cfg.various_folder, sub)
            } else {
                sub
            }
        }
        Bucket::Singles => {
            if cfg.singles_under_artist && !info.album_artist.trim().is_empty() {
                render_folder_path(
                    &format!("{{albumArtist}}/{}/{{title}}", cfg.singles_folder),
                    &fields,
                    &opts,
                )
            } else {
                let sub = render_folder_path(
                    &format!("{}/{{albumArtist}} - {{title}}", cfg.singles_folder),
                    &fields,
                    &opts,
                );
                format!("{}/{}", cfg.various_folder, sub)
            }
        }
        Bucket::Various => render_folder_path(
            &format!("{}/{{album}} ({{year}})", cfg.various_folder),
            &fields,
            &opts,
        ),
        Bucket::Normal => render_folder_path(&cfg.folder_schema, &fields, &opts),
    };
    // Keep live/bootleg albums distinct from the studio release.
    let suffix = match info.recording {
        Recording::Live | Recording::Bootleg => format!(" ({})", info.recording.as_str()),
        _ => String::new(),
    };
    let rendered = format!("{rendered}{suffix}");
    if rendered.is_empty() {
        folder_name(&fields.album_artist).to_string()
    } else {
        rendered
    }
}

/// Best-effort track number/title from the file name (e.g. "01 - Dream On.flac")
/// used as a fallback when embedded tags are missing.
fn parse_file_name(name: &str) -> (Option<u32>, String) {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let trimmed = stem.trim();
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        (None, trimmed.to_string())
    } else if let Some(rest) = trimmed.strip_prefix(&digits) {
        let rest = rest.trim_start_matches(['-', ' ', '_', '.', ')', '(', '[']);
        (digits.parse().ok(), rest.to_string())
    } else {
        (digits.parse().ok(), trimmed.to_string())
    }
}

/// Build the full change plan for one album. Reads embedded tags on demand
/// (discovery keeps them deferred so a full-library scan stays fast).
pub fn build_plan(album: &AlbumDir, cfg: &Config, root: &Path) -> AlbumPlan {
    let mut album = album.clone();
    for t in &mut album.tracks {
        if t.tags.is_none() {
            t.tags = read_tags(&root.join(&album.dir).join(&t.name));
        }
    }
    let info = album_info_with_nfo(&album, cfg, root);
    let bucket = classify(&info, cfg);
    let first_title = album
        .tracks
        .first()
        .and_then(|t| t.tags.as_ref())
        .map(|t| t.title.clone())
        .unwrap_or_else(|| parse_file_name(&album.tracks[0].name).1);

    let target_dir = target_album_dir(bucket, &info, cfg, &first_title);
    let mut plan = AlbumPlan {
        bucket,
        current_dir: album.dir.clone(),
        target_dir: target_dir.clone(),
        ..Default::default()
    };

    // Album-level sidecars move with the folder whenever the folder itself moves.
    if !album.dir.eq_ignore_ascii_case(&target_dir) {
        if let Ok(entries) = std::fs::read_dir(root.join(&album.dir)) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if DIR_SIDECARS.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
                    plan.dir_sidecars.push(name);
                }
            }
        }
    }

    let mut seen_targets: HashSet<String> = HashSet::new();
    for track in &album.tracks {
        let (tag_track, tag_disc, tag_title) = match &track.tags {
            Some(t) => (t.track, t.disc, t.title.clone()),
            None => {
                let (n, title) = parse_file_name(&track.name);
                (n, None, title)
            }
        };
        let (name_track, _) = parse_file_name(&track.name);
        let title = if tag_title.is_empty() {
            parse_file_name(&track.name).1
        } else {
            tag_title.clone()
        };
        let artist = track
            .tags
            .as_ref()
            .map(|t| t.artist.clone())
            .unwrap_or_else(|| info.album_artist.clone());
        let track_recording = track.tags.as_ref().map(|t| t.recording).unwrap_or(Recording::Studio);

        let fields = TemplateFields {
            track: tag_track.or(name_track),
            disc: tag_disc,
            title,
            artist,
            album_artist: info.album_artist.clone(),
            album: info.album.clone(),
            year: info.year,
            genre: info.genre.clone(),
            recording: track_recording.as_str().to_string(),
            mbid: track.tags.as_ref().map(|t| t.mbid_recording.clone()).unwrap_or_default(),
        };
        let mut new_name = render_file_name(&cfg.file_schema, &fields, &sanitize_opts(cfg));
        // A live/bootleg track is a distinct recording: append "(Live)"/"(Bootleg)"
        // so it can never collide with (or be confused with) the studio version.
        if cfg.preserve_recording_type
            && matches!(track_recording, Recording::Live | Recording::Bootleg)
            && !new_name
                .to_ascii_lowercase()
                .contains(&track_recording.as_str().to_ascii_lowercase())
        {
            new_name = format!("{new_name} ({})", track_recording.as_str());
        }
        let ext = track
            .name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        let new_file = format!("{new_name}.{ext}");

        let from = join_rel(&album.dir, &track.name);
        let to = join_rel(&target_dir, &new_file);

        // Same path (case-insensitive: filesystems here are effectively
        // case-insensitive or the name is identical) -> keep.
        if from.eq_ignore_ascii_case(&to) {
            plan.keeps += 1;
            continue;
        }

        // Collision detection: the target exists and is a different file.
        let target_exists = !target_dir.is_empty() && root.join(&to).exists();
        let dup = !seen_targets.insert(to.clone().to_ascii_lowercase());
        if target_exists || dup {
            plan.skipped.push((from.clone(), if dup { "duplicate target name".into() } else { "target file already exists".into() }));
            continue;
        }

        // Sidecar files that share the source stem.
        let src_stem = track.name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&track.name);
        let mut sidecars = Vec::new();
        if cfg.rename_sidecars {
            for e in std::fs::read_dir(root.join(&album.dir)).into_iter().flatten().flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if is_sidecar(&n) && n.starts_with(src_stem) {
                    sidecars.push(n);
                }
            }
        }

        plan.moves.push(FileMove { from, to, sidecars });
    }
    plan
}

fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Apply a plan: create target dirs, move files and sidecars, optionally prune
/// empty source dirs. Only for `mode: apply`; dry-run never calls this.
pub fn apply_plan(root: &Path, plan: &AlbumPlan, prune: bool) -> Result<(), String> {
    let mut errors = Vec::new();
    for m in &plan.moves {
        let to_path = root.join(&m.to);
        if let Some(parent) = to_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                errors.push(format!("mkdir {}: {e}", parent.display()));
                continue;
            }
        }
        let from_path = root.join(&m.from);
        if let Err(e) = std::fs::rename(&from_path, &to_path) {
            errors.push(format!("move {} -> {}: {e}", m.from, m.to));
            continue;
        }
        // Move sidecars from the same source directory.
        if let Some(src_dir) = from_path.parent() {
            for sc in &m.sidecars {
                let _ = std::fs::rename(src_dir.join(sc), src_dir.join(sc));
            }
        }
    }
    // Move album-level sidecars (album.nfo, cover.jpg, ...) into the target dir.
    if !plan.current_dir.eq_ignore_ascii_case(&plan.target_dir) && !plan.dir_sidecars.is_empty() {
        let src_dir = root.join(&plan.current_dir);
        let dst_dir = root.join(&plan.target_dir);
        if let Err(e) = std::fs::create_dir_all(&dst_dir) {
            errors.push(format!("mkdir {}: {e}", dst_dir.display()));
        } else {
            for name in &plan.dir_sidecars {
                let from = src_dir.join(name);
                if from.exists() {
                    if let Err(e) = std::fs::rename(&from, dst_dir.join(name)) {
                        errors.push(format!("move sidecar {name}: {e}"));
                    }
                }
            }
        }
    }
    if prune {
        prune_empty_dirs(root, &plan.current_dir);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Remove the album's source directory (and empty ancestors) after moves.
fn prune_empty_dirs(root: &Path, rel: &str) {
    let mut cur = root.join(rel);
    loop {
        if !cur.is_dir() {
            break;
        }
        let Ok(mut rd) = std::fs::read_dir(&cur) else { break };
        if rd.next().is_some() {
            break; // not empty
        }
        let _ = std::fs::remove_dir(&cur);
        let Some(parent) = cur.parent() else { break };
        if parent == root {
            break;
        }
        cur = parent.to_path_buf();
    }
}

/// Keep the compiler happy about unused import in wasm builds where tests run
/// on the host only.
#[allow(dead_code)]
fn _pathbuf_unused(_: PathBuf) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::fs;

    /// Create a small library fixture on disk (tagless files, so classification
    /// uses folder/file-name fallbacks). Unique temp dir per call so parallel
    /// tests don't race.
    fn fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nd-organizer-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (album, files) in [
            ("Artist A/Album One", &["01 - Dream On.flac", "02 - Walk This Way.mp3"] as &[&str]),
            ("Various Artists/Greatest Hits", &["01 - Hit One.flac"]),
            ("OST Sample/Original Soundtrack", &["01 - Theme.flac"]),
            ("Lone Singer/Single Song", &["01 - Only One.flac"]),
        ] {
            for f in files {
                let p = dir.join(album).join(f);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(&p, b"not really audio").unwrap();
            }
        }
        dir
    }

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn discovers_all_albums() {
        let root = fixture("discover");
        let albums = discover_albums(&root);
        assert_eq!(albums.len(), 4, "expected 4 album dirs: {albums:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn classify_pure_rules() {
        let c = cfg();
        let info = |album: &str, artist: &str, genre: &str, tracks: usize, multi: bool| AlbumInfo {
            album: album.into(),
            album_artist: artist.into(),
            year: None,
            genre: genre.into(),
            track_count: tracks,
            distinct_artists: if multi { vec!["A".into(), "B".into()] } else { vec![] },
            recording: Recording::Studio,
        };

        assert_eq!(classify(&info("OST", "X", "Soundtrack", 12, false), &c), Bucket::Soundtrack);
        assert_eq!(classify(&info("Album", "X", "OST", 12, false), &c), Bucket::Soundtrack);
        assert_eq!(classify(&info("Greatest", "Various Artists", "", 20, false), &c), Bucket::Various);
        assert_eq!(classify(&info("Comp", "Various", "Rock", 14, false), &c), Bucket::Various);
        assert_eq!(classify(&info("Multi", "", "Rock", 10, true), &c), Bucket::Various);
        assert_eq!(classify(&info("Solo", "X", "Rock", 1, false), &c), Bucket::Singles);
        assert_eq!(classify(&info("Partial", "X", "Rock", 2, false), &c), Bucket::Singles);
        assert_eq!(classify(&info("Real", "X", "Rock", 12, false), &c), Bucket::Normal);
        // Incomplete-album threshold is configurable.
        let mut c2 = cfg();
        c2.incomplete_album_min_tracks = 5;
        assert_eq!(classify(&info("Partial", "X", "Rock", 4, false), &c2), Bucket::Singles);
    }

    #[test]
    fn album_info_falls_back_to_folder_names() {
        let root = fixture("info");
        let albums = discover_albums(&root);
        let one = albums.iter().find(|a| a.dir.ends_with("Album One")).unwrap();
        let info = album_info(one);
        assert_eq!(info.album, "Album One"); // folder name fallback
        assert_eq!(info.track_count, 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn plan_builds_for_tagless_fixture() {
        let root = fixture("plan");
        let albums = discover_albums(&root);
        let c = cfg();
        for a in &albums {
            let p = build_plan(a, &c, &root);
            assert!(!p.target_dir.is_empty(), "target empty for {:?}", a.dir);
            if p.bucket == Bucket::Soundtrack {
                // Soundtracks nest under the various-artist root by default.
                let expect = format!("{}/{}", c.various_folder, c.soundtrack_folder);
                assert!(p.target_dir.starts_with(&expect), "{:?}", p.target_dir);
            } else {
                // Tagless albums have no artist, so singles fall back under various.
                assert_eq!(p.bucket, Bucket::Singles);
                let expect = format!("{}/{}", c.various_folder, c.singles_folder);
                assert!(p.target_dir.starts_with(&expect), "{:?}", p.target_dir);
            }
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn singles_land_under_artist_when_known() {
        let mut c = cfg();
        let info = AlbumInfo {
            album: "Crazy".into(),
            album_artist: "Beyonce".into(),
            year: None,
            genre: String::new(),
            track_count: 1,
            distinct_artists: vec![],
            recording: Recording::Studio,
        };
        assert_eq!(target_album_dir(Bucket::Singles, &info, &c, "Crazy"), "Beyonce/Singles/Crazy");
        // Unknown artist falls back to the various-artist singles area.
        let mut anon = info.clone();
        anon.album_artist = String::new();
        assert_eq!(
            target_album_dir(Bucket::Singles, &anon, &c, "Crazy"),
            "Various Artist/Singles/ - Crazy"
        );
        // Live single stays distinct from the studio version.
        let mut live = info.clone();
        live.recording = Recording::Live;
        assert_eq!(
            target_album_dir(Bucket::Singles, &live, &c, "Crazy"),
            "Beyonce/Singles/Crazy (Live)"
        );
        // Opting out restores the flat various-artist layout.
        c.singles_under_artist = false;
        assert_eq!(
            target_album_dir(Bucket::Singles, &info, &c, "Crazy"),
            "Various Artist/Singles/Beyonce - Crazy"
        );
    }

    #[test]
    fn soundtrack_nesting_respects_config() {
        let root = fixture("nested");
        let albums = discover_albums(&root);
        let soundtrack = albums.iter().find(|a| a.dir.contains("Soundtrack")).unwrap();
        let mut c = cfg();
        assert!(target_album_dir(Bucket::Soundtrack, &album_info_with_nfo(soundtrack, &c, &root), &c, "Theme")
            .starts_with(&format!("{}/{}", c.various_folder, c.soundtrack_folder)));
        c.nest_buckets_under_various = false;
        assert!(target_album_dir(Bucket::Soundtrack, &album_info_with_nfo(soundtrack, &c, &root), &c, "Theme")
            .starts_with(&c.soundtrack_folder));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_respects_excludes() {
        let root = fixture("excl");
        let albums = discover_albums_skip(
            &root,
            true,
            &["Artist A".to_string(), "OST Sample/*".to_string()],
            0,
            0,
        );
        assert_eq!(albums.len(), 2, "expected 2 remaining albums: {albums:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nfo_fills_missing_fields() {
        let root = std::env::temp_dir().join(format!("nd-organizer-nfo-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Artist Folder")).unwrap();
        fs::write(root.join("Artist Folder/01 - Track.flac"), b"x").unwrap();
        fs::write(
            root.join("Artist Folder/album.nfo"),
            r#"<album><title>Great Album</title><albumartist>Real Artist</albumartist><year>1987</year><genre>Synthpop</genre></album>"#,
        )
        .unwrap();

        let albums = discover_albums(&root);
        assert_eq!(albums.len(), 1);
        let nfo = nfo::read_album_nfo(&root.join("Artist Folder"));
        assert!(nfo.is_some(), "read_album_nfo returned None");
        let info = album_info_with_nfo(&albums[0], &Config::default(), &root);
        assert_eq!(info.album, "Great Album");
        assert_eq!(info.album_artist, "Real Artist");
        assert_eq!(info.year, Some(1987));
        assert_eq!(info.genre, "Synthpop");

        // With no tags/nfo the folder name is the only fallback.
        let info_no_nfo = album_info(&albums[0]);
        assert_eq!(info_no_nfo.album, "Artist Folder");
        assert!(info_no_nfo.album_artist.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_moves_files_and_cleans_source() {
        let root = fixture("apply");
        let albums = discover_albums(&root);
        let c = cfg();
        let mut any_move = false;
        for a in &albums {
            let plan = build_plan(a, &c, &root);
            if !plan.moves.is_empty() {
                any_move = true;
                apply_plan(&root, &plan, true).unwrap();
            }
        }
        assert!(any_move, "fixture should produce at least one move");
        // Old album dirs must be pruned after their content moved out.
        assert!(!root.join("Artist A/Album One").exists());
        assert!(!root.join("Various Artists/Greatest Hits").exists());
        assert!(!root.join("Lone Singer/Single Song").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn render_sidecar_moves_only_matching_stem() {
        let root = std::env::temp_dir().join(format!("nd-organizer-sidecar-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Album")).unwrap();
        fs::write(root.join("Album/01 - Song.flac"), b"x").unwrap();
        fs::write(root.join("Album/01 - Song.lrc"), b"x").unwrap();
        fs::write(root.join("Album/album.nfo"), b"<album/>").unwrap();
        fs::write(root.join("Album/cover.jpg"), b"x").unwrap();
        let albums = discover_albums(&root);
        assert_eq!(albums.len(), 1);
        let plan = build_plan(&albums[0], &Config::default(), &root);
        assert_eq!(plan.moves.len(), 1);
        // Track-scoped sidecar follows its stem.
        assert_eq!(plan.moves[0].sidecars, vec!["01 - Song.lrc"]);
        // Album-level sidecars move with the folder.
        assert!(plan.dir_sidecars.contains(&"album.nfo".to_string()));
        assert!(plan.dir_sidecars.contains(&"cover.jpg".to_string()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_moves_dir_sidecars() {
        let root = std::env::temp_dir().join(format!("nd-organizer-dirsc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Some Artist/Some Album")).unwrap();
        fs::write(root.join("Some Artist/Some Album/01 - Song.flac"), b"x").unwrap();
        fs::write(root.join("Some Artist/Some Album/album.nfo"), b"<album/>").unwrap();
        let albums = discover_albums(&root);
        let plan = build_plan(&albums[0], &Config::default(), &root);
        assert!(plan.dir_sidecars.contains(&"album.nfo".to_string()));
        apply_plan(&root, &plan, true).unwrap();
        // The nfo moved with the album and the source dir is gone.
        let target = root.join(&plan.target_dir);
        assert!(target.join("album.nfo").exists());
        assert!(!root.join("Some Artist/Some Album/album.nfo").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_can_be_disabled() {
        let root = std::env::temp_dir().join(format!("nd-organizer-noprune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Some Artist/Some Album")).unwrap();
        fs::write(root.join("Some Artist/Some Album/01 - Song.flac"), b"x").unwrap();
        let albums = discover_albums(&root);
        let plan = build_plan(&albums[0], &Config::default(), &root);
        assert!(!plan.moves.is_empty());
        apply_plan(&root, &plan, false).unwrap();
        // Files moved but the now-empty source folder is left in place.
        assert!(root.join(&plan.target_dir).join(plan.moves[0].to.rsplit('/').next().unwrap()).exists());
        assert!(root.join("Some Artist/Some Album").exists());
        let _ = fs::remove_dir_all(&root);
    }
}

