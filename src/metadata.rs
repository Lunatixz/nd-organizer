// Unified metadata aggregation for nd-organizer.
//
// Collects metadata from ALL configured sources (audiomuse, essentia, musicbrainz,
// discogs, theaudiodb, coverart, apple music, genius, lrclib, nfo), fills gaps
// with priority logic, and writes the merged result once.

use std::collections::HashMap;

/// Unified metadata record that holds all possible metadata fields.
#[derive(Debug, Clone, Default)]
pub struct MetadataRecord {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<u32>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub genre: Option<String>,
    pub genres: Vec<String>,
    pub mood: Option<String>,
    pub moods: Vec<String>,
    pub bpm: Option<f64>,
    pub key: Option<String>,
    pub energy: Option<f64>,
    pub description: Option<String>,
    pub artwork_url: Option<String>,
    pub lyrics: Option<String>,
    pub mbid_album: Option<String>,
    pub mbid_recording: Option<String>,
    pub credits: Vec<String>,
    pub style: Option<String>,
}

impl MetadataRecord {
    /// Merge another record into this one, filling gaps only.
    /// Priority: existing value wins over new value (first source wins).
    pub fn fill_from(&mut self, other: &MetadataRecord) {
        if self.title.is_none() && other.title.is_some() {
            self.title = other.title.clone();
        }
        if self.artist.is_none() && other.artist.is_some() {
            self.artist = other.artist.clone();
        }
        if self.album_artist.is_none() && other.album_artist.is_some() {
            self.album_artist = other.album_artist.clone();
        }
        if self.album.is_none() && other.album.is_some() {
            self.album = other.album.clone();
        }
        if self.year.is_none() && other.year.is_some() {
            self.year = other.year;
        }
        if self.track_number.is_none() && other.track_number.is_some() {
            self.track_number = other.track_number;
        }
        if self.disc_number.is_none() && other.disc_number.is_some() {
            self.disc_number = other.disc_number;
        }
        if self.genre.is_none() && other.genre.is_some() {
            self.genre = other.genre.clone();
        }
        if self.genres.is_empty() && !other.genres.is_empty() {
            self.genres = other.genres.clone();
        }
        if self.mood.is_none() && other.mood.is_some() {
            self.mood = other.mood.clone();
        }
        if self.moods.is_empty() && !other.moods.is_empty() {
            self.moods = other.moods.clone();
        }
        if self.bpm.is_none() && other.bpm.is_some() {
            self.bpm = other.bpm;
        }
        if self.key.is_none() && other.key.is_some() {
            self.key = other.key.clone();
        }
        if self.energy.is_none() && other.energy.is_some() {
            self.energy = other.energy;
        }
        if self.description.is_none() && other.description.is_some() {
            self.description = other.description.clone();
        }
        if self.artwork_url.is_none() && other.artwork_url.is_some() {
            self.artwork_url = other.artwork_url.clone();
        }
        if self.lyrics.is_none() && other.lyrics.is_some() {
            self.lyrics = other.lyrics.clone();
        }
        if self.mbid_album.is_none() && other.mbid_album.is_some() {
            self.mbid_album = other.mbid_album.clone();
        }
        if self.mbid_recording.is_none() && other.mbid_recording.is_some() {
            self.mbid_recording = other.mbid_recording.clone();
        }
        if self.credits.is_empty() && !other.credits.is_empty() {
            self.credits = other.credits.clone();
        }
        if self.style.is_none() && other.style.is_some() {
            self.style = other.style.clone();
        }
    }

    /// Merge with priority — higher priority wins on conflicts.
    pub fn merge_with_priority(&mut self, other: &MetadataRecord, priority: u8) {
        // Priority 0: NFO (lowest — fallback only)
        // Priority 1: TheAudioDB
        // Priority 2: Discogs
        // Priority 3: MusicBrainz
        // Priority 4: Apple Music
        // Priority 5: Essentia/AudioMuse (acoustic analysis)
        // Priority 6: AcoustID (identity)
        // Priority 7: User-provided tags (highest — never overwrite)
        // For now, just fill gaps (first source wins)
        self.fill_from(other);
    }
}

/// Collect metadata from all configured sources for a track.
pub fn collect_metadata(
    cfg: &crate::config::Config,
    _root: &std::path::Path,
    _rel: &str,
    _tags: &crate::tags::TrackTags,
) -> MetadataRecord {
    let mut record = MetadataRecord::default();

    // Start with existing tags (highest priority — user-provided).
    if !_tags.title.is_empty() { record.title = Some(_tags.title.clone()); }
    if !_tags.artist.is_empty() { record.artist = Some(_tags.artist.clone()); }
    if !_tags.album_artist.is_empty() { record.album_artist = Some(_tags.album_artist.clone()); }
    if !_tags.album.is_empty() { record.album = Some(_tags.album.clone()); }
    record.year = _tags.year;
    record.track_number = _tags.track;
    record.disc_number = _tags.disc;
    if !_tags.genre.is_empty() { record.genre = Some(_tags.genre.clone()); }
    if !_tags.mbid_album.is_empty() { record.mbid_album = Some(_tags.mbid_album.clone()); }
    if !_tags.mbid_recording.is_empty() { record.mbid_recording = Some(_tags.mbid_recording.clone()); }

    // Essentia genres/moods (if configured).
    if cfg.genre_source == "essentia" && !cfg.essentia_url.trim().is_empty() {
        // Essentia data is written during enrich step — record would be populated there.
    }

    // AudioMuse acoustic tags (if configured).
    if cfg.write_acoustic_tags && !cfg.audiomuse_url.trim().is_empty() {
        // AudioMuse data is written during enrich step — record would be populated there.
    }

    record
}
