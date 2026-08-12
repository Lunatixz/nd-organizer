// Typed configuration for the plugin.
//
// Navidrome stores plugin config as a flat map of key -> string (the keys are
// the manifest schema property names). We parse that map into a typed struct.
// `Config::load()` reads from the host; `Config::from_map()` is pure so unit
// tests can construct configs without a running host.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    DryRun,
    Apply,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcoustIdMode {
    Disabled,
    Lookup,
    Fingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimarySource {
    Lidarr,
    MusicBrainz,
    Itunes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LidarrMode {
    Disabled,
    MetadataOnly,
    MetadataPlusRescan,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    // General
    pub mode: Mode,
    /// Primary library (first entry of `libraries`).
    pub library_id: i32,
    /// All libraries to organize. Empty = organize every library the plugin is
    /// granted access to (the Navidrome "Library Access" permission is the
    /// authority); non-empty = only these IDs (a subset override).
    pub libraries: Vec<i32>,
    pub run_on_startup: bool,
    pub schedule_cron: String,
    pub max_albums_per_run: usize,
    /// Max directory entries a single pass scans before giving up (0 = unlimited).
    pub max_scan_entries: usize,
    /// Albums processed per background task (small batches keep the plugin lean).
    pub albums_per_task: usize,
    /// Files scanned (tags read) per scan task chunk.
    pub files_per_scan_task: usize,
    /// Only run when nothing is playing (player resources come first).
    pub run_only_when_idle: bool,

    // Favorites sync (Navidrome hub <-> Last.fm loved tracks)
    pub favorites_sync_enabled: bool,
    pub favorites_sync_lastfm: bool,
    pub favorites_sync_max: usize,

    // Playback stats (plays/skips weighting + Top Picks playlist)
    pub playback_stats_enabled: bool,
    pub stats_poll_minutes: i32,
    pub top_picks_count: usize,
    pub skip_threshold_percent: i32,
    /// Publish frequently-skipped track IDs to the Subsonic filter proxy so
    /// they are excluded from client playback. No files are moved.
    pub skip_ignore_enabled: bool,
    pub skip_ignore_ratio: f64,
    /// Base URL of the Subsonic filter proxy (e.g. http://nd-organizer-proxy:4534).
    pub filter_url: String,
    /// When set, the next run pass rolls back that run's changes instead of
    /// organizing. Empty disables rollback.
    pub rollback_run_id: String,
    /// Optional URL to POST each run's report to (a hosted log: ntfy, Gotify,
    /// Discord webhook, Loki push endpoint, ...).
    pub log_webhook_url: String,
    /// Optional token sent as X-Token (and Authorization: Bearer) header.
    pub log_webhook_token: String,

    // Persistence backend. "host" = Navidrome-managed SQLite KVStore (default).
    // "mysql" = the plugin's KVStore state lives in the user's MySQL/MariaDB via
    // the mysql sidecar (persistenceUrl), with the connection details below.
    pub persistence_backend: String,
    pub persistence_url: String,
    pub mysql_host: String,
    pub mysql_port: u16,
    pub mysql_name: String,
    pub mysql_user: String,
    pub mysql_password: String,

    // Classification
    pub soundtrack_folder: String,
    pub various_folder: String,
    pub singles_folder: String,
    /// Nest soundtracks under the various-artist folder. Singles are governed by
    /// `singles_under_artist` (they go under the single's artist).
    pub nest_buckets_under_various: bool,
    pub incomplete_album_min_tracks: usize,
    pub classify_from_mb: bool,

    // Sidecar metadata (Kodi-style NFO)
    pub read_nfo: bool,
    pub write_nfo: bool,

    // Schemas
    pub folder_schema: String,
    pub file_schema: String,
    pub rename_sidecars: bool,
    pub illegal_char_replacement: String,
    pub max_name_length: usize,
    pub prune_empty_dirs: bool,
    pub skip_hidden_files: bool,
    pub preserve_recording_type: bool,
    pub singles_under_artist: bool,
    /// Route single-track and incomplete albums to the Singles folder. Off =
    /// they stay as normal albums in their own folder.
    pub singles_enabled: bool,
    /// Comma-separated keywords that mark a track as filler (intro/outro/...).
    /// Used to report filler tracks; the filter proxy drops them by title.
    pub filler_keywords: String,
    /// Path prefixes/globs under the library root that must never be touched.
    pub exclude_paths: Vec<String>,
    /// Snapshot previous tags (and original .nfo) to plugin storage before any
    /// tag/nfo write. Metadata-only, never copies audio bytes.
    pub backup_before_write: bool,
    /// Days to keep metadata backups before pruning. 0 = keep forever.
    pub backup_retention_days: i32,

    // Identity verification
    pub verify_identity: bool,
    pub min_confidence: f64,
    pub skip_unverified: bool,
    pub acoustid_mode: AcoustIdMode,
    pub acoustid_api_key: String,
    /// URL of the AcoustID fingerprint sidecar (Docker), e.g. http://acoustid:8097
    pub acoustid_url: String,

    // Metadata sources
    pub primary_source: PrimarySource,
    pub musicbrainz_token: String,
    pub lastfm_api_key: String,
    pub lastfm_user: String,
    /// Last.fm API secret (write methods need an api_sig).
    pub lastfm_api_secret: String,
    /// Last.fm account password (used once to obtain a session key).
    pub lastfm_password: String,
    pub genre_from: String,
    pub overwrite_existing_tags: bool,
    pub write_playcount: bool,

    // Artwork
    pub embed_artwork: bool,
    pub write_cover_jpg: bool,
    pub overwrite_art: bool,
    pub artwork_priority: String,
    pub artwork_front: bool,
    pub artwork_back: bool,
    pub artwork_cd: bool,
    pub artwork_booklet: bool,

    // Lyrics
    pub download_lyrics: bool,
    pub lyrics_format: String,

    // Lidarr
    pub lidarr_url: String,
    pub lidarr_api_key: String,
    pub lidarr_mode: LidarrMode,
    pub write_tags_for_tracked: bool,
    pub lidarr_force_search_incomplete: bool,
    /// Use Lidarr's naming schema (from /config/naming) for folders/files
    /// instead of the plugin's folderSchema/fileSchema.
    pub use_lidarr_naming_schema: bool,

    // AudioMuse-AI
    pub audiomuse_url: String,
    pub audiomuse_token: String,
    pub notify_audiomuse_after_run: bool,
    pub write_acoustic_tags: bool,

    // Scanning
    pub scan_user: String,
    pub trigger_scan_after_run: bool,
    pub scan_after_album: bool,
    pub scan_after_tag_write: bool,
    pub scan_debounce_seconds: i32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mode: Mode::DryRun,
            library_id: 1,
            libraries: Vec::new(),
            run_on_startup: false,
            schedule_cron: String::new(),
            max_albums_per_run: 100,
            max_scan_entries: 3000,
            albums_per_task: 5,
            files_per_scan_task: 200,
            run_only_when_idle: true,
            favorites_sync_enabled: false,
            favorites_sync_lastfm: true,
            favorites_sync_max: 500,
            playback_stats_enabled: false,
            stats_poll_minutes: 5,
            top_picks_count: 50,
            skip_threshold_percent: 30,
            skip_ignore_enabled: false,
            skip_ignore_ratio: 0.6,
            filter_url: String::new(),
            rollback_run_id: String::new(),
            log_webhook_url: String::new(),
            log_webhook_token: String::new(),
            persistence_backend: "host".into(),
            persistence_url: String::new(),
            mysql_host: String::new(),
            mysql_port: 3306,
            mysql_name: String::new(),
            mysql_user: String::new(),
            mysql_password: String::new(),
            soundtrack_folder: "Sound Tracks".into(),
            various_folder: "Various Artist".into(),
            singles_folder: "Singles".into(),
            nest_buckets_under_various: true,
            incomplete_album_min_tracks: 3,
            classify_from_mb: true,
            read_nfo: true,
            write_nfo: false,
            folder_schema: "{albumArtist}/{album} ({year})".into(),
            file_schema: "{track:02} - {title}".into(),
            rename_sidecars: true,
            illegal_char_replacement: "_".into(),
            max_name_length: 180,
            prune_empty_dirs: true,
            skip_hidden_files: true,
            preserve_recording_type: true,
            singles_under_artist: true,
            singles_enabled: true,
            filler_keywords: "intro,outro,interlude,transition,prelude,postlude,christmas,commercial,skit,instrumental,interview".into(),
            exclude_paths: Vec::new(),
            backup_before_write: true,
            backup_retention_days: 30,
            verify_identity: true,
            min_confidence: 0.6,
            skip_unverified: true,
            acoustid_mode: AcoustIdMode::Fingerprint,
            acoustid_api_key: String::new(),
            acoustid_url: String::new(),
            primary_source: PrimarySource::MusicBrainz,
            musicbrainz_token: String::new(),
            lastfm_api_key: String::new(),
            lastfm_user: String::new(),
            lastfm_api_secret: String::new(),
            lastfm_password: String::new(),
            genre_from: "musicbrainz".into(),
            overwrite_existing_tags: false,
            write_playcount: false,
            embed_artwork: true,
            write_cover_jpg: false,
            overwrite_art: false,
            artwork_priority: "coverartarchive".into(),
            artwork_front: true,
            artwork_back: false,
            artwork_cd: false,
            artwork_booklet: false,
            download_lyrics: false,
            lyrics_format: "lrc".into(),
            lidarr_url: String::new(),
            lidarr_api_key: String::new(),
            lidarr_mode: LidarrMode::Disabled,
            write_tags_for_tracked: false,
            lidarr_force_search_incomplete: false,
            use_lidarr_naming_schema: false,
            audiomuse_url: String::new(),
            audiomuse_token: String::new(),
            notify_audiomuse_after_run: true,
            write_acoustic_tags: false,
            scan_user: String::new(),
            trigger_scan_after_run: true,
            scan_after_album: true,
            scan_after_tag_write: false,
            scan_debounce_seconds: 0,
        }
    }
}

impl Config {
    /// Read config from the Navidrome host. Only available on the wasm target
    /// (host-service imports don't exist in host test builds).
    #[cfg(target_arch = "wasm32")]
    pub fn load() -> Result<Config, String> {
        let mut map = HashMap::new();
        // Pull every declared key from the host config service. Keys that are
        // absent are simply not inserted; from_map applies defaults.
        for key in [
            "mode",
            "libraryId",
            "libraries",
            "runOnStartup",
            "scheduleCron",
            "maxAlbumsPerRun",
            "maxScanEntries",
            "albumsPerTask",
            "filesPerScanTask",
            "runOnlyWhenIdle",
            "favoritesSyncEnabled",
            "favoritesSyncLastfm",
            "favoritesSyncMax",
            "playbackStatsEnabled",
            "statsPollMinutes",
            "topPicksCount",
            "skipThresholdPercent",
            "skipIgnoreEnabled",
            "skipIgnoreRatio",
            "filterUrl",
            "rollbackRunId",
            "logWebhookUrl",
            "logWebhookToken",
            "persistenceBackend",
            "persistenceUrl",
            "mysqlHost",
            "mysqlPort",
            "mysqlName",
            "mysqlUser",
            "mysqlPassword",
            "soundtrackFolder",
            "variousFolder",
            "singlesFolder",
            "nestBucketsUnderVarious",
            "incompleteAlbumMinTracks",
            "classifyFromMB",
            "readNfo",
            "writeNfo",
            "folderSchema",
            "fileSchema",
            "renameSidecars",
            "illegalCharReplacement",
            "maxNameLength",
            "pruneEmptyDirs",
            "skipHiddenFiles",
            "preserveRecordingType",
            "singlesUnderArtist",
            "singlesEnabled",
            "fillerKeywords",
            "excludePaths",
            "backupBeforeWrite",
            "backupRetentionDays",
            "illegalCharReplacement",
            "maxNameLength",
            "pruneEmptyDirs",
            "skipHiddenFiles",
            "verifyIdentity",
            "minConfidence",
            "skipUnverified",
            "acoustidMode",
            "acoustidApiKey",
            "acoustidUrl",
            "primarySource",
            "musicbrainzToken",
            "lastfmApiKey",
            "lastfmUser",
            "lastfmApiSecret",
            "lastfmPassword",
            "genreFrom",
            "overwriteExistingTags",
            "writePlaycount",
            "embedArtwork",
            "writeCoverJpg",
            "overwriteArt",
            "artworkPriority",
            "artworkFront",
            "artworkBack",
            "artworkCd",
            "artworkBooklet",
            "downloadLyrics",
            "lyricsFormat",
            "lidarrUrl",
            "lidarrApiKey",
            "lidarrMode",
            "writeTagsForTracked",
            "lidarrForceSearchIncomplete",
            "useLidarrNamingSchema",
            "audiomuseUrl",
            "audiomuseToken",
            "notifyAudiomuseAfterRun",
            "writeAcousticTags",
            "scanUser",
            "triggerScanAfterRun",
            "scanAfterAlbum",
            "scanAfterTagWrite",
            "scanDebounceSeconds",
        ] {
            if let Ok(Some(v)) = nd_pdk::host::config::get(key) {
                map.insert(key.to_string(), v);
            }
        }
        Ok(Config::from_map(&map))
    }

    pub fn from_map(map: &HashMap<String, String>) -> Config {
        let mut c = Config::default();
        if let Some(v) = map.get("mode") {
            c.mode = if v == "apply" {
                Mode::Apply
            } else {
                Mode::DryRun
            };
        }
        // Multi-library: `libraries` wins if present; `libraryId` is the single
        // library fallback. Handles JSON array, comma list or single value.
        let libs = map.get("libraries").or_else(|| map.get("libraryId"));
        if let Some(v) = libs {
            let parsed = parse_library_list(v);
            if !parsed.is_empty() {
                c.libraries = parsed;
            }
        }
        c.library_id = c.libraries.first().copied().unwrap_or(1);
        c.run_on_startup = bool(map, "runOnStartup", c.run_on_startup);
        if let Some(v) = map.get("scheduleCron") {
            c.schedule_cron = v.clone();
        }
        if let Some(v) = map.get("rollbackRunId") {
            c.rollback_run_id = v.clone();
        }
        if let Some(v) = map.get("logWebhookUrl") {
            c.log_webhook_url = v.clone();
        }
        if let Some(v) = map.get("logWebhookToken") {
            c.log_webhook_token = v.clone();
        }
        if let Some(v) = map.get("persistenceBackend") {
            c.persistence_backend = v.trim().to_string();
        }
        if let Some(v) = map.get("persistenceUrl") {
            c.persistence_url = v.trim().to_string();
        }
        if let Some(v) = map.get("mysqlHost") {
            c.mysql_host = v.trim().to_string();
        }
        if let Some(v) = map.get("mysqlPort") {
            c.mysql_port = v.trim().parse().unwrap_or(c.mysql_port);
        }
        if let Some(v) = map.get("mysqlName") {
            c.mysql_name = v.trim().to_string();
        }
        if let Some(v) = map.get("mysqlUser") {
            c.mysql_user = v.trim().to_string();
        }
        if let Some(v) = map.get("mysqlPassword") {
            c.mysql_password = v.to_string();
        }
        if let Some(v) = map.get("maxAlbumsPerRun") {
            c.max_albums_per_run = v.trim().parse().unwrap_or(c.max_albums_per_run);
        }
        if let Some(v) = map.get("maxScanEntries") {
            c.max_scan_entries = v.trim().parse().unwrap_or(c.max_scan_entries);
        }
        if let Some(v) = map.get("albumsPerTask") {
            c.albums_per_task = v.trim().parse().unwrap_or(c.albums_per_task);
        }
        if let Some(v) = map.get("filesPerScanTask") {
            c.files_per_scan_task = v.trim().parse().unwrap_or(c.files_per_scan_task);
        }
        c.run_only_when_idle = bool(map, "runOnlyWhenIdle", c.run_only_when_idle);
        c.favorites_sync_enabled = bool(map, "favoritesSyncEnabled", c.favorites_sync_enabled);
        c.favorites_sync_lastfm = bool(map, "favoritesSyncLastfm", c.favorites_sync_lastfm);
        if let Some(v) = map.get("favoritesSyncMax") {
            c.favorites_sync_max = v.trim().parse().unwrap_or(c.favorites_sync_max);
        }
        c.playback_stats_enabled = bool(map, "playbackStatsEnabled", c.playback_stats_enabled);
        if let Some(v) = map.get("statsPollMinutes") {
            c.stats_poll_minutes = v.trim().parse().unwrap_or(c.stats_poll_minutes);
        }
        if let Some(v) = map.get("topPicksCount") {
            c.top_picks_count = v.trim().parse().unwrap_or(c.top_picks_count);
        }
        if let Some(v) = map.get("skipThresholdPercent") {
            c.skip_threshold_percent = v.trim().parse().unwrap_or(c.skip_threshold_percent);
        }
        c.skip_ignore_enabled = bool(map, "skipIgnoreEnabled", c.skip_ignore_enabled);
        if let Some(v) = map.get("skipIgnoreRatio") {
            c.skip_ignore_ratio = v.trim().parse().unwrap_or(c.skip_ignore_ratio);
        }
        if let Some(v) = map.get("filterUrl") {
            c.filter_url = v.trim().to_string();
        }
        if let Some(v) = map.get("soundtrackFolder") {
            c.soundtrack_folder = v.clone();
        }
        if let Some(v) = map.get("variousFolder") {
            c.various_folder = v.clone();
        }
        if let Some(v) = map.get("singlesFolder") {
            c.singles_folder = v.clone();
        }
        c.nest_buckets_under_various =
            bool(map, "nestBucketsUnderVarious", c.nest_buckets_under_various);
        c.read_nfo = bool(map, "readNfo", c.read_nfo);
        c.write_nfo = bool(map, "writeNfo", c.write_nfo);
        if let Some(v) = map.get("incompleteAlbumMinTracks") {
            c.incomplete_album_min_tracks =
                v.trim().parse().unwrap_or(c.incomplete_album_min_tracks);
        }
        c.classify_from_mb = bool(map, "classifyFromMB", c.classify_from_mb);
        if let Some(v) = map.get("folderSchema") {
            c.folder_schema = v.clone();
        }
        if let Some(v) = map.get("fileSchema") {
            c.file_schema = v.clone();
        }
        c.rename_sidecars = bool(map, "renameSidecars", c.rename_sidecars);
        if let Some(v) = map.get("illegalCharReplacement") {
            c.illegal_char_replacement = v.clone();
        }
        if let Some(v) = map.get("maxNameLength") {
            c.max_name_length = v.trim().parse().unwrap_or(c.max_name_length);
        }
        c.prune_empty_dirs = bool(map, "pruneEmptyDirs", c.prune_empty_dirs);
        c.skip_hidden_files = bool(map, "skipHiddenFiles", c.skip_hidden_files);
        c.preserve_recording_type = bool(map, "preserveRecordingType", c.preserve_recording_type);
        c.singles_enabled = bool(map, "singlesEnabled", c.singles_enabled);
        c.singles_under_artist = bool(map, "singlesUnderArtist", c.singles_under_artist);
        if let Some(v) = map.get("fillerKeywords") {
            c.filler_keywords = v.clone();
        }
        if let Some(v) = map.get("excludePaths") {
            let parsed = parse_string_list(v);
            if !parsed.is_empty() {
                c.exclude_paths = parsed;
            }
        }
        c.backup_before_write = bool(map, "backupBeforeWrite", c.backup_before_write);
        if let Some(v) = map.get("backupRetentionDays") {
            c.backup_retention_days = v.trim().parse().unwrap_or(c.backup_retention_days);
        }
        c.verify_identity = bool(map, "verifyIdentity", c.verify_identity);
        if let Some(v) = map.get("minConfidence") {
            c.min_confidence = v.trim().parse().unwrap_or(c.min_confidence);
        }
        c.skip_unverified = bool(map, "skipUnverified", c.skip_unverified);
        if let Some(v) = map.get("acoustidMode") {
            c.acoustid_mode = match v.as_str() {
                "disabled" => AcoustIdMode::Disabled,
                "lookup" => AcoustIdMode::Lookup,
                _ => AcoustIdMode::Fingerprint,
            };
        }
        if let Some(v) = map.get("acoustidApiKey") {
            c.acoustid_api_key = v.clone();
        }
        if let Some(v) = map.get("acoustidUrl") {
            c.acoustid_url = v.clone();
        }
        if let Some(v) = map.get("primarySource") {
            c.primary_source = match v.as_str() {
                "lidarr" => PrimarySource::Lidarr,
                "itunes" => PrimarySource::Itunes,
                _ => PrimarySource::MusicBrainz,
            };
        }
        if let Some(v) = map.get("musicbrainzToken") {
            c.musicbrainz_token = v.clone();
        }
        if let Some(v) = map.get("lastfmApiKey") {
            c.lastfm_api_key = v.clone();
        }
        if let Some(v) = map.get("lastfmUser") {
            c.lastfm_user = v.clone();
        }
        if let Some(v) = map.get("lastfmApiSecret") {
            c.lastfm_api_secret = v.clone();
        }
        if let Some(v) = map.get("lastfmPassword") {
            c.lastfm_password = v.clone();
        }
        if let Some(v) = map.get("genreFrom") {
            c.genre_from = v.clone();
        }
        c.overwrite_existing_tags = bool(map, "overwriteExistingTags", c.overwrite_existing_tags);
        c.write_playcount = bool(map, "writePlaycount", c.write_playcount);
        c.embed_artwork = bool(map, "embedArtwork", c.embed_artwork);
        c.write_cover_jpg = bool(map, "writeCoverJpg", c.write_cover_jpg);
        c.overwrite_art = bool(map, "overwriteArt", c.overwrite_art);
        if let Some(v) = map.get("artworkPriority") {
            c.artwork_priority = v.clone();
        }
        c.artwork_front = bool(map, "artworkFront", c.artwork_front);
        c.artwork_back = bool(map, "artworkBack", c.artwork_back);
        c.artwork_cd = bool(map, "artworkCd", c.artwork_cd);
        c.artwork_booklet = bool(map, "artworkBooklet", c.artwork_booklet);
        c.download_lyrics = bool(map, "downloadLyrics", c.download_lyrics);
        if let Some(v) = map.get("lyricsFormat") {
            c.lyrics_format = v.clone();
        }
        if let Some(v) = map.get("lidarrUrl") {
            c.lidarr_url = v.clone();
        }
        if let Some(v) = map.get("lidarrApiKey") {
            c.lidarr_api_key = v.clone();
        }
        if let Some(v) = map.get("lidarrMode") {
            c.lidarr_mode = match v.as_str() {
                "metadataOnly" => LidarrMode::MetadataOnly,
                "metadataPlusRescan" => LidarrMode::MetadataPlusRescan,
                _ => LidarrMode::Disabled,
            };
        }
        c.write_tags_for_tracked = bool(map, "writeTagsForTracked", c.write_tags_for_tracked);
        c.lidarr_force_search_incomplete = bool(
            map,
            "lidarrForceSearchIncomplete",
            c.lidarr_force_search_incomplete,
        );
        c.use_lidarr_naming_schema = bool(map, "useLidarrNamingSchema", c.use_lidarr_naming_schema);
        if let Some(v) = map.get("audiomuseUrl") {
            c.audiomuse_url = v.clone();
        }
        if let Some(v) = map.get("audiomuseToken") {
            c.audiomuse_token = v.clone();
        }
        c.notify_audiomuse_after_run =
            bool(map, "notifyAudiomuseAfterRun", c.notify_audiomuse_after_run);
        c.write_acoustic_tags = bool(map, "writeAcousticTags", c.write_acoustic_tags);
        if let Some(v) = map.get("scanUser") {
            c.scan_user = v.clone();
        }
        c.trigger_scan_after_run = bool(map, "triggerScanAfterRun", c.trigger_scan_after_run);
        c.scan_after_album = bool(map, "scanAfterAlbum", c.scan_after_album);
        c.scan_after_tag_write = bool(map, "scanAfterTagWrite", c.scan_after_tag_write);
        if let Some(v) = map.get("scanDebounceSeconds") {
            c.scan_debounce_seconds = v.trim().parse().unwrap_or(c.scan_debounce_seconds);
        }
        c
    }
}

fn bool(map: &HashMap<String, String>, key: &str, default: bool) -> bool {
    match map.get(key).map(|s| s.trim().to_ascii_lowercase()) {
        Some(v) if v == "true" || v == "1" || v == "yes" => true,
        Some(v) if v == "false" || v == "0" || v == "no" => false,
        _ => default,
    }
}

/// Parse a string-list value that may be a JSON array ("[\"a\",\"b\"]") or a
/// comma-separated list ("a, b").
fn parse_string_list(v: &str) -> Vec<String> {
    let trimmed = v.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Ok(json) = serde_json::from_str::<Vec<String>>(trimmed) {
        return json
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse a library-id value that may be a JSON array ("[1,2]"), a comma list
/// ("1, 2") or a single integer ("1").
fn parse_library_list(v: &str) -> Vec<i32> {
    let trimmed = v.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Ok(json) = serde_json::from_str::<Vec<serde_json::Value>>(trimmed) {
        return json
            .iter()
            .filter_map(|x| {
                x.as_i64()
                    .or_else(|| x.as_str().and_then(|s| s.trim().parse().ok()))
            })
            .filter(|&n| n > 0)
            .map(|n| n as i32)
            .collect();
    }
    trimmed
        .split(',')
        .filter_map(|s| s.trim().parse::<i32>().ok())
        .filter(|&n| n > 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults_apply_when_empty() {
        let c = Config::from_map(&HashMap::new());
        assert_eq!(c.mode, Mode::DryRun);
        assert_eq!(c.folder_schema, "{albumArtist}/{album} ({year})");
        assert_eq!(c.file_schema, "{track:02} - {title}");
        assert_eq!(c.soundtrack_folder, "Sound Tracks");
        assert_eq!(c.incomplete_album_min_tracks, 3);
        assert!(c.scan_after_album);
    }

    #[test]
    fn parses_explicit_values() {
        let c = Config::from_map(&map(&[
            ("mode", "apply"),
            ("libraryId", "2"),
            ("runOnStartup", "true"),
            ("folderSchema", "{albumArtist}/{album}"),
            ("incompleteAlbumMinTracks", "5"),
            ("acoustidMode", "lookup"),
            ("primarySource", "lidarr"),
            ("lidarrMode", "metadataOnly"),
            ("minConfidence", "0.8"),
            ("scanDebounceSeconds", "5"),
            ("nestBucketsUnderVarious", "false"),
            ("readNfo", "true"),
            ("writeNfo", "true"),
            ("artworkBack", "true"),
            ("artworkFront", "false"),
            ("downloadLyrics", "true"),
            ("lyricsFormat", "lrc"),
        ]));
        assert_eq!(c.mode, Mode::Apply);
        assert_eq!(c.library_id, 2);
        assert_eq!(c.libraries, vec![2]);
        assert!(c.run_on_startup);
        assert_eq!(c.folder_schema, "{albumArtist}/{album}");
        assert_eq!(c.incomplete_album_min_tracks, 5);
        assert_eq!(c.acoustid_mode, AcoustIdMode::Lookup);
        assert_eq!(c.primary_source, PrimarySource::Lidarr);
        assert_eq!(c.lidarr_mode, LidarrMode::MetadataOnly);
        assert!((c.min_confidence - 0.8).abs() < f64::EPSILON);
        assert_eq!(c.scan_debounce_seconds, 5);
        assert!(!c.nest_buckets_under_various);
        assert!(c.read_nfo && c.write_nfo);
        assert!(c.artwork_back && !c.artwork_front);
        assert!(c.download_lyrics);
        assert_eq!(c.lyrics_format, "lrc");
    }

    #[test]
    fn multi_library_parsing() {
        assert_eq!(parse_library_list("[1,2,3]"), vec![1, 2, 3]);
        assert_eq!(parse_library_list("1, 2, 3"), vec![1, 2, 3]);
        assert_eq!(parse_library_list("7"), vec![7]);
        assert_eq!(parse_library_list(""), Vec::<i32>::new());
        assert_eq!(parse_library_list("garbage"), Vec::<i32>::new());

        // `libraries` wins over `libraryId`.
        let c = Config::from_map(&map(&[("libraries", "[2,5]"), ("libraryId", "9")]));
        assert_eq!(c.libraries, vec![2, 5]);
        assert_eq!(c.library_id, 2);

        // Single `libraryId` fallback.
        let c = Config::from_map(&map(&[("libraryId", "4")]));
        assert_eq!(c.libraries, vec![4]);
        assert_eq!(c.library_id, 4);
    }

    #[test]
    fn parses_extra_user_options() {
        let c = Config::from_map(&map(&[
            ("pruneEmptyDirs", "false"),
            ("skipHiddenFiles", "false"),
            ("illegalCharReplacement", "-"),
            ("maxNameLength", "120"),
            ("lidarrForceSearchIncomplete", "true"),
            ("preserveRecordingType", "false"),
            ("singlesUnderArtist", "false"),
            ("singlesEnabled", "false"),
            ("backupBeforeWrite", "false"),
            ("useLidarrNamingSchema", "true"),
            ("excludePaths", "[\"inbox\", \"Downloads/*\"]"),
        ]));
        assert!(!c.prune_empty_dirs);
        assert!(!c.skip_hidden_files);
        assert_eq!(c.illegal_char_replacement, "-");
        assert_eq!(c.max_name_length, 120);
        assert!(c.lidarr_force_search_incomplete);
        assert!(!c.preserve_recording_type);
        assert!(!c.singles_under_artist);
        assert!(!c.singles_enabled);
        assert!(!c.backup_before_write);
        assert!(c.use_lidarr_naming_schema);
        assert_eq!(c.exclude_paths, vec!["inbox", "Downloads/*"]);
    }

    #[test]
    fn parses_comma_string_list() {
        assert_eq!(parse_string_list("a, b , c"), vec!["a", "b", "c"]);
        assert_eq!(parse_string_list("[\"x\",\"y\"]"), vec!["x", "y"]);
        assert_eq!(parse_string_list(""), Vec::<String>::new());
    }

    #[test]
    fn parses_filler_and_playback_stats() {
        let c = Config::from_map(&map(&[
            ("fillerKeywords", "intro,outro,cold open"),
            ("playbackStatsEnabled", "true"),
            ("statsPollMinutes", "10"),
            ("skipThresholdPercent", "25"),
            ("topPicksCount", "80"),
        ]));
        assert_eq!(c.filler_keywords, "intro,outro,cold open");
        assert!(c.playback_stats_enabled);
        assert_eq!(c.stats_poll_minutes, 10);
        assert_eq!(c.skip_threshold_percent, 25);
        assert_eq!(c.top_picks_count, 80);
        // Defaults are opt-in (off).
        let d = Config::from_map(&HashMap::new());
        assert!(!d.playback_stats_enabled);
        assert_eq!(d.skip_threshold_percent, 30);
    }

    #[test]
    fn parses_persistence_mysql_fields() {
        let c = Config::from_map(&map(&[
            ("persistenceBackend", "mysql"),
            ("persistenceUrl", "http://nd-organizer-mysql:8098"),
            ("mysqlHost", "db.internal"),
            ("mysqlPort", "3307"),
            ("mysqlName", "navidrome_plugins"),
            ("mysqlUser", "plugin"),
            ("mysqlPassword", "s3cret"),
        ]));
        assert_eq!(c.persistence_backend, "mysql");
        assert_eq!(c.persistence_url, "http://nd-organizer-mysql:8098");
        assert_eq!(c.mysql_host, "db.internal");
        assert_eq!(c.mysql_port, 3307);
        assert_eq!(c.mysql_name, "navidrome_plugins");
        assert_eq!(c.mysql_user, "plugin");
        assert_eq!(c.mysql_password, "s3cret");
        // Default backend is the Navidrome host KVStore.
        let d = Config::from_map(&HashMap::new());
        assert_eq!(d.persistence_backend, "host");
        assert_eq!(d.mysql_port, 3306);
    }
}

