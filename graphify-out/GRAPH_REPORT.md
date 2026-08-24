# Graph Report - nd-organizer  (2026-08-24)

## Corpus Check
- 75 files · ~89,604 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 1743 nodes · 3308 edges · 141 communities (140 shown, 1 thin omitted)
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS · INFERRED: 8 edges (avg confidence: 0.85)
- Token cost: 0 input · 0 output

## Community Hubs (Navigation)
- Configuration & Modes
- WASM Lifecycle & Scheduling
- Album Organization & Grouping
- Star Ratings & Playback Stats
- MusicBrainz Metadata Provider
- Cache System
- KV Store Operations
- State Machine & Circuit Breaker
- Webhook HTTP Server
- Favorites & Last.fm Sync
- Plugin Manifest & Settings
- PDK WebSocket Protocol
- Path Templates & Formatting
- Logging & Store Backend
- Task Queue & Background Jobs
- Scrobbler & ListenBrainz
- WebSocket Connection Mgmt
- Lyrics Fetching (LRCLIB)
- Subsonic Scrobble Retrieve
- JSON Schema & Validation
- Module Cluster 20
- Module Cluster 21
- Module Cluster 22
- Module Cluster 23
- Module Cluster 24
- Module Cluster 25
- Module Cluster 26
- Module Cluster 27
- Module Cluster 28
- Module Cluster 29
- Module Cluster 30
- Module Cluster 31
- Module Cluster 32
- Module Cluster 33
- Module Cluster 34
- Module Cluster 35
- Module Cluster 36
- Module Cluster 37
- Module Cluster 38
- Module Cluster 39
- Module Cluster 40
- Module Cluster 41
- Module Cluster 42
- Module Cluster 43
- Module Cluster 44
- Module Cluster 45
- Module Cluster 46
- Module Cluster 47
- Module Cluster 48
- Module Cluster 49
- Module Cluster 50
- Module Cluster 51
- Module Cluster 52
- Module Cluster 53
- Module Cluster 54
- Module Cluster 55
- Module Cluster 56
- Module Cluster 57
- Module Cluster 58
- Module Cluster 59
- Module Cluster 60
- Module Cluster 61
- Module Cluster 62
- Module Cluster 63
- Module Cluster 64
- Module Cluster 65
- Module Cluster 66
- Module Cluster 67
- Module Cluster 68
- Module Cluster 69
- Module Cluster 70
- Module Cluster 71
- Module Cluster 72
- Module Cluster 73
- Module Cluster 74
- Module Cluster 75
- Module Cluster 76
- Module Cluster 77
- Module Cluster 78
- Module Cluster 79
- Module Cluster 80
- Module Cluster 81
- Module Cluster 82
- Module Cluster 83
- Module Cluster 84
- Module Cluster 85
- Module Cluster 86
- Module Cluster 87
- Module Cluster 88
- Module Cluster 89
- Module Cluster 90
- Module Cluster 91
- Module Cluster 92
- Module Cluster 93
- Module Cluster 94
- Module Cluster 95
- Module Cluster 96
- Module Cluster 97
- Module Cluster 98
- Module Cluster 99
- Module Cluster 100
- Module Cluster 101
- Module Cluster 102
- Module Cluster 103
- Module Cluster 104
- Module Cluster 105
- Module Cluster 106
- Module Cluster 107
- Module Cluster 108
- Module Cluster 109
- Module Cluster 110
- Module Cluster 111
- Module Cluster 112
- Module Cluster 113
- Module Cluster 114
- Module Cluster 115
- Module Cluster 116
- Module Cluster 117
- Module Cluster 118
- Module Cluster 119
- Module Cluster 120
- Module Cluster 121
- Module Cluster 122
- Module Cluster 123
- Module Cluster 124
- Module Cluster 125
- Module Cluster 126
- Module Cluster 127
- Module Cluster 128
- Module Cluster 129
- Module Cluster 130
- Module Cluster 131
- Module Cluster 132
- Module Cluster 133
- Module Cluster 134
- Module Cluster 135
- Module Cluster 136

## God Nodes (most connected - your core abstractions)
1. `Config` - 112 edges
2. `TrackTags` - 25 edges
3. `build_group_plan()` - 21 edges
4. `build_plan()` - 19 edges
5. `run_pass()` - 18 edges
6. `esc()` - 17 edges
7. `AlbumInfo` - 16 edges
8. `lidarr_send()` - 15 edges
9. `plan_step()` - 15 edges
10. `discover_albums()` - 14 edges

## Surprising Connections (you probably didn't know these)
- `NdOrganizer` --implements--> `CallbackProvider`  [EXTRACTED]
  src/lib.rs → pdk/nd-pdk-capabilities/src/scheduler.rs
- `NdOrganizer` --implements--> `TaskExecuteProvider`  [EXTRACTED]
  src/lib.rs → pdk/nd-pdk-capabilities/src/taskworker.rs
- `lidarr_send()` --references--> `HTTPResponse`  [EXTRACTED]
  src/lidarr.rs → pdk/nd-pdk-host/src/nd_host_http.rs
- `nd-organizer` --depends_on--> `nd-pdk`  [EXTRACTED]
  Cargo.toml → pdk/nd-pdk/Cargo.toml
- `NdOrganizer` --implements--> `InitProvider`  [EXTRACTED]
  src/lib.rs → pdk/nd-pdk-capabilities/src/lifecycle.rs

## Import Cycles
- None detected.

## Communities (141 total, 1 thin omitted)

### Community 0 - "Configuration & Modes"
Cohesion: 0.05
Nodes (99): AcoustIdMode, bool(), Config, defaults_apply_when_empty(), LidarrMode, map(), Mode, multi_library_parsing() (+91 more)

### Community 1 - "WASM Lifecycle & Scheduling"
Cohesion: 0.05
Nodes (76): InitProvider, CallbackProvider, Error, Display, Formatter, Into, Result, Self (+68 more)

### Community 2 - "Album Organization & Grouping"
Cohesion: 0.08
Nodes (73): album_fields(), album_info(), album_info_falls_back_to_folder_names(), album_info_from_tags(), album_info_raw(), album_info_with_nfo(), AlbumDir, AlbumInfo (+65 more)

### Community 3 - "Star Ratings & Playback Stats"
Cohesion: 0.09
Nodes (53): album_rating_for(), all_weights(), apply_band(), bump(), describe(), describe_is_detailed_even_when_idle(), extract_path_from_song(), hard_exclude() (+45 more)

### Community 4 - "MusicBrainz Metadata Provider"
Cohesion: 0.06
Nodes (37): AlbumImagesProvider, AlbumImagesResponse, AlbumInfoProvider, AlbumInfoResponse, AlbumRequest, ArtistBiographyProvider, ArtistBiographyResponse, ArtistImagesProvider (+29 more)

### Community 5 - "Cache System"
Cohesion: 0.13
Nodes (40): CacheGetBytesRequest, CacheGetBytesResponse, CacheGetFloatRequest, CacheGetFloatResponse, CacheGetIntRequest, CacheGetIntResponse, CacheGetStringRequest, CacheGetStringResponse (+32 more)

### Community 6 - "KV Store Operations"
Cohesion: 0.15
Nodes (37): delete(), delete_by_prefix(), deserialize(), get(), get_many(), get_storage_used(), has(), KVStoreDeleteByPrefixRequest (+29 more)

### Community 7 - "State Machine & Circuit Breaker"
Cohesion: 0.13
Nodes (29): acoustid_stage(), AcoustidStage, ApplyRecord, backup_key(), backup_tag_state(), cache_meta(), file_index_key(), file_index_key_is_bounded_for_deep_paths() (+21 more)

### Community 8 - "Webhook HTTP Server"
Cohesion: 0.10
Nodes (23): _count_songs(), filter_json(), forward(), Handler, is_filler_title(), is_skip_heavy(), is_song(), _limit_skip_heavy() (+15 more)

### Community 9 - "Favorites & Last.fm Sync"
Cohesion: 0.17
Nodes (32): api_sig(), is_loved(), lastfm_get(), lastfm_love(), lastfm_loved(), lastfm_post(), listenbrainz_scrobble(), LovedTrack (+24 more)

### Community 10 - "Plugin Manifest & Settings"
Cohesion: 0.06
Nodes (32): author, reason, config, schema, description, reason, requiredHosts, maxSize (+24 more)

### Community 11 - "PDK WebSocket Protocol"
Cohesion: 0.09
Nodes (22): BinaryMessageProvider, CloseProvider, deserialize(), Error, ErrorProvider, OnBinaryMessageRequest, OnCloseRequest, OnErrorRequest (+14 more)

### Community 12 - "Path Templates & Formatting"
Cohesion: 0.15
Nodes (24): empty_components_are_dropped(), fields(), folder_schema_nests_and_sanitizes(), is_forbidden(), is_reserved_windows(), long_names_capped(), missing_numeric_renders_empty(), opts() (+16 more)

### Community 13 - "Logging & Store Backend"
Cohesion: 0.21
Nodes (20): append_log(), build_backend(), Kv, migrate_mysql_chunk(), MigrateStatus, mysql_backend(), mysql_migration_needed(), MysqlDb (+12 more)

### Community 14 - "Task Queue & Background Jobs"
Cohesion: 0.18
Nodes (27): cancel(), clear_queue(), create_queue(), deserialize(), enqueue(), get(), QueueConfig, D (+19 more)

### Community 15 - "Scrobbler & ListenBrainz"
Cohesion: 0.11
Nodes (15): Error, IsAuthorizedRequest, NowPlayingRequest, PlaybackReportRequest, Display, Formatter, Into, Result (+7 more)

### Community 16 - "WebSocket Connection Mgmt"
Cohesion: 0.19
Nodes (23): close_connection(), connect(), deserialize(), D, Error, HashMap, Ok, Option (+15 more)

### Community 17 - "Lyrics Fetching (LRCLIB)"
Cohesion: 0.11
Nodes (14): Error, GetLyricsRequest, GetLyricsResponse, Lyrics, LyricsText, Display, Formatter, Into (+6 more)

### Community 18 - "Subsonic Scrobble Retrieve"
Cohesion: 0.24
Nodes (20): get_first_timestamp(), get_last_timestamp(), get_scrobble_count(), get_scrobbles(), Error, Option, Result, String (+12 more)

### Community 19 - "JSON Schema & Validation"
Cohesion: 0.10
Nodes (20): type, default, description, title, type, description, title, type (+12 more)

### Community 20 - "Module Cluster 20"
Cohesion: 0.14
Nodes (14): current_mode(), delete_playlist(), Handler, latest_status(), list_playlists(), playlist_dir(), Latest mode the plugin reported (dryRun / apply), so text-only entries can be…, Write a response body, swallowing broken-pipe/reset errors - a client (browser… (+6 more)

### Community 21 - "Module Cluster 21"
Cohesion: 0.15
Nodes (11): acoustid_lookup(), fpcalc(), Handler, MemHandler, BaseHTTPRequestHandler, Compute loudness with ffmpeg's EBU R128: integrated loudness (LUFS) and true…, Write a response body, swallowing broken-pipe/reset errors - a client that…, Post a liveness heartbeat to the webhook dashboard (WEBHOOK_URL). (+3 more)

### Community 22 - "Module Cluster 22"
Cohesion: 0.20
Nodes (12): add_stations(), db_connect(), generate_id(), get_timestamp(), Handler, list_stations(), MemHandler, BaseHTTPRequestHandler (+4 more)

### Community 23 - "Module Cluster 23"
Cohesion: 0.26
Nodes (17): Acoustic, circuit_clear(), circuit_mark_failed(), fetch(), headers(), probe_up(), RawAcoustic, re_sync() (+9 more)

### Community 24 - "Module Cluster 24"
Cohesion: 0.25
Nodes (17): esc(), NfoAlbum, NfoArtist, parse_album_nfo(), parse_artist_nfo(), parse_year(), parses_album_nfo(), parses_artist_nfo() (+9 more)

### Community 25 - "Module Cluster 25"
Cohesion: 0.32
Nodes (17): atomic_write(), detect_recording(), fill_missing_from_mb(), read_tags(), Recording, Option, Path, Result (+9 more)

### Community 26 - "Module Cluster 26"
Cohesion: 0.23
Nodes (15): FnOnce, cached(), circuit_check(), circuit_clear(), circuit_mark_failed(), circuit_open(), circuit_probe(), circuit_since() (+7 more)

### Community 27 - "Module Cluster 27"
Cohesion: 0.26
Nodes (16): ArtworkGetAlbumUrlRequest, ArtworkGetAlbumUrlResponse, ArtworkGetArtistUrlRequest, ArtworkGetArtistUrlResponse, ArtworkGetPlaylistUrlRequest, ArtworkGetPlaylistUrlResponse, ArtworkGetTrackUrlRequest, ArtworkGetTrackUrlResponse (+8 more)

### Community 28 - "Module Cluster 28"
Cohesion: 0.24
Nodes (16): deserialize(), HTTPRequest, HTTPResponse, HTTPSendRequest, HTTPSendResponse, D, Error, HashMap (+8 more)

### Community 29 - "Module Cluster 29"
Cohesion: 0.24
Nodes (16): call(), call_raw(), deserialize(), D, Error, Ok, Option, Result (+8 more)

### Community 30 - "Module Cluster 30"
Cohesion: 0.18
Nodes (16): _fhist_html(), _fmt_bytes(), _fmt_ms(), _fmt_ts(), latest_plans(), playback_html(), Playback' panel: what is playing right now, playcounts + star ratings, and what…, Render a sidecar's recent filtered-track history as a list. (+8 more)

### Community 31 - "Module Cluster 31"
Cohesion: 0.23
Nodes (13): MimeType, PictureType, ArtKind, embed(), fetch(), has_embedded(), Option, Path (+5 more)

### Community 32 - "Module Cluster 32"
Cohesion: 0.17
Nodes (9): FindSonicPathRequest, GetSonicSimilarTracksRequest, ArtistRef, HashMap, Option, String, Vec, SongRef (+1 more)

### Community 33 - "Module Cluster 33"
Cohesion: 0.19
Nodes (9): connect(), ensure_schema(), handle(), Handler, MemHandler, BaseHTTPRequestHandler, Write a response body, swallowing broken-pipe/reset errors - a client that…, Post a liveness heartbeat to the webhook dashboard (WEBHOOK_URL). (+1 more)

### Community 34 - "Module Cluster 34"
Cohesion: 0.25
Nodes (14): ConfigGetIntRequest, ConfigGetIntResponse, ConfigGetRequest, ConfigGetResponse, ConfigKeysRequest, ConfigKeysResponse, get(), get_int() (+6 more)

### Community 35 - "Module Cluster 35"
Cohesion: 0.29
Nodes (13): cancel_schedule(), Error, Option, Result, String, schedule_one_time(), schedule_recurring(), SchedulerCancelScheduleRequest (+5 more)

### Community 36 - "Module Cluster 36"
Cohesion: 0.38
Nodes (13): lookup(), MbRelease, MbTrack, parse_release_tracks(), parse_releases(), RawRelease, RawReleaseGroup, release_tracks() (+5 more)

### Community 37 - "Module Cluster 37"
Cohesion: 0.19
Nodes (5): analyze_audio(), Handler, MemHandler, BaseHTTPRequestHandler, Analyze an audio file and return genre/mood predictions.

### Community 38 - "Module Cluster 38"
Cohesion: 0.18
Nodes (13): activity_entry(), entry_summary(), esc(), integrations_html(), mode_chip(), plans_html(), Structured album plans: kind, artist/album, target folder, every move., One Activity row, rendered richly: mode chip (DRY RUN/APPLY on every entry),… (+5 more)

### Community 39 - "Module Cluster 39"
Cohesion: 0.33
Nodes (11): get_all_libraries(), get_library(), Library, LibraryGetAllLibrariesResponse, LibraryGetLibraryRequest, LibraryGetLibraryResponse, Error, Option (+3 more)

### Community 40 - "Module Cluster 40"
Cohesion: 0.21
Nodes (12): _docker_logs(), _fetch_logs(), _octo_fiesta_card(), _octo_fiesta_config(), _octo_fiesta_health(), _octo_fiesta_logs(), Fetch each sidecar's /status + /logs (cached 30s) and render rich cards so this…, Latest octoFiestaUrl / octoFiestaProvider from the plugin status POST. (+4 more)

### Community 41 - "Module Cluster 41"
Cohesion: 0.18
Nodes (11): description, options, title, type, description, options, title, type (+3 more)

### Community 42 - "Module Cluster 42"
Cohesion: 0.18
Nodes (11): skipContentMode, default, description, enum, title, type, exclude, half (+3 more)

### Community 43 - "Module Cluster 43"
Cohesion: 0.20
Nodes (4): Vec, SonicMatch, SonicSimilarity, SonicSimilarityResponse

### Community 44 - "Module Cluster 44"
Cohesion: 0.40
Nodes (10): get_admins(), get_users(), Error, Option, Result, String, Vec, User (+2 more)

### Community 45 - "Module Cluster 45"
Cohesion: 0.22
Nodes (7): Error, Display, Formatter, Into, Result, Self, String

### Community 46 - "Module Cluster 46"
Cohesion: 0.33
Nodes (9): match_songs(), MatcherMatchSongsRequest, MatcherMatchSongsResponse, MatchOptions, Error, Option, Result, String (+1 more)

### Community 47 - "Module Cluster 47"
Cohesion: 0.25
Nodes (7): Error, Display, Formatter, Into, Result, Self, String

### Community 48 - "Module Cluster 48"
Cohesion: 0.39
Nodes (8): fetch(), Lyrics, Option, Path, Result, String, urlenc(), write_sidecar()

### Community 49 - "Module Cluster 49"
Cohesion: 0.25
Nodes (8): last_action_html(), latest_actions(), now_panel(), pipeline_html(), One-line 'last action' ticker for the Now-doing hero., Horizontal pipeline stepper: Scan -> Verify -> Group -> Plan -> Preview/Apply…, The 'Current activity' hero: a plain-English line about the current action, a…, The most recent `actions` list across any status/report entry.

### Community 50 - "Module Cluster 50"
Cohesion: 0.29
Nodes (7): default, description, items, title, type, type, excludePaths

### Community 51 - "Module Cluster 51"
Cohesion: 0.29
Nodes (7): default, description, maximum, minimum, title, type, maxNameLength

### Community 52 - "Module Cluster 52"
Cohesion: 0.29
Nodes (7): default, description, maximum, minimum, title, type, minConfidence

### Community 53 - "Module Cluster 53"
Cohesion: 0.29
Nodes (7): default, description, maximum, minimum, title, type, mysqlPort

### Community 54 - "Module Cluster 54"
Cohesion: 0.29
Nodes (7): skipHeavyRatio, default, description, maximum, minimum, title, type

### Community 55 - "Module Cluster 55"
Cohesion: 0.29
Nodes (7): skipThresholdPercent, default, description, maximum, minimum, title, type

### Community 56 - "Module Cluster 56"
Cohesion: 0.29
Nodes (7): starFullPlayPercent, default, description, maximum, minimum, title, type

### Community 57 - "Module Cluster 57"
Cohesion: 0.29
Nodes (7): starHalfPlayPercent, default, description, maximum, minimum, title, type

### Community 58 - "Module Cluster 58"
Cohesion: 0.29
Nodes (7): starMinSamples, default, description, maximum, minimum, title, type

### Community 59 - "Module Cluster 59"
Cohesion: 0.29
Nodes (7): statsPollMinutes, default, description, maximum, minimum, title, type

### Community 60 - "Module Cluster 60"
Cohesion: 0.33
Nodes (5): get_storage_path(), Error, Result, String, StorageGetStoragePathResponse

### Community 61 - "Module Cluster 61"
Cohesion: 0.33
Nodes (6): default, description, oneOf, title, type, acoustidMode

### Community 62 - "Module Cluster 62"
Cohesion: 0.33
Nodes (6): default, description, minimum, title, type, albumsPerTask

### Community 63 - "Module Cluster 63"
Cohesion: 0.33
Nodes (6): default, description, oneOf, title, type, artworkPriority

### Community 64 - "Module Cluster 64"
Cohesion: 0.33
Nodes (6): default, description, minimum, title, type, backupRetentionDays

### Community 65 - "Module Cluster 65"
Cohesion: 0.33
Nodes (6): default, description, minimum, title, type, favoritesSyncMax

### Community 66 - "Module Cluster 66"
Cohesion: 0.33
Nodes (6): default, description, minimum, title, type, filesPerScanTask

### Community 67 - "Module Cluster 67"
Cohesion: 0.33
Nodes (6): default, description, oneOf, title, type, genreFrom

### Community 68 - "Module Cluster 68"
Cohesion: 0.33
Nodes (6): default, description, minimum, title, type, incompleteAlbumMinTracks

### Community 69 - "Module Cluster 69"
Cohesion: 0.33
Nodes (6): default, description, oneOf, title, type, lidarrMode

### Community 70 - "Module Cluster 70"
Cohesion: 0.33
Nodes (6): default, description, oneOf, title, type, lyricsFormat

### Community 71 - "Module Cluster 71"
Cohesion: 0.33
Nodes (6): default, description, minimum, title, type, maxAlbumsPerRun

### Community 72 - "Module Cluster 72"
Cohesion: 0.33
Nodes (6): default, description, oneOf, title, type, mode

### Community 73 - "Module Cluster 73"
Cohesion: 0.33
Nodes (6): default, description, oneOf, title, type, persistenceBackend

### Community 74 - "Module Cluster 74"
Cohesion: 0.33
Nodes (6): default, description, oneOf, title, type, primarySource

### Community 75 - "Module Cluster 75"
Cohesion: 0.33
Nodes (6): rollbackRetentionDays, default, description, minimum, title, type

### Community 76 - "Module Cluster 76"
Cohesion: 0.33
Nodes (6): topPicksCount, default, description, minimum, title, type

### Community 77 - "Module Cluster 77"
Cohesion: 0.40
Nodes (6): load_log(), Load only the most recent events from the log file, then self-clean it. Reading…, Return the last `n` non-empty lines of a file without loading it all. Reads…, Keep the log file bounded: if it's grown large, rewrite it to only the newest…, read_tail(), _self_clean_log()

### Community 78 - "Module Cluster 78"
Cohesion: 0.40
Nodes (5): default, description, title, type, acoustidUrl

### Community 79 - "Module Cluster 79"
Cohesion: 0.40
Nodes (5): default, description, title, type, artworkBack

### Community 80 - "Module Cluster 80"
Cohesion: 0.40
Nodes (5): default, description, title, type, artworkBooklet

### Community 81 - "Module Cluster 81"
Cohesion: 0.40
Nodes (5): default, description, title, type, artworkCd

### Community 82 - "Module Cluster 82"
Cohesion: 0.40
Nodes (5): default, description, title, type, artworkFront

### Community 83 - "Module Cluster 83"
Cohesion: 0.40
Nodes (5): default, description, title, type, backupBeforeWrite

### Community 84 - "Module Cluster 84"
Cohesion: 0.40
Nodes (5): default, description, title, type, classifyFromMB

### Community 85 - "Module Cluster 85"
Cohesion: 0.40
Nodes (5): default, description, title, type, downloadLyrics

### Community 86 - "Module Cluster 86"
Cohesion: 0.40
Nodes (5): default, description, title, type, embedArtwork

### Community 87 - "Module Cluster 87"
Cohesion: 0.40
Nodes (5): default, description, title, type, fileSchema

### Community 88 - "Module Cluster 88"
Cohesion: 0.40
Nodes (5): default, description, title, type, fillerKeywords

### Community 89 - "Module Cluster 89"
Cohesion: 0.40
Nodes (5): default, description, title, type, filterUrl

### Community 90 - "Module Cluster 90"
Cohesion: 0.40
Nodes (5): default, description, title, type, folderSchema

### Community 91 - "Module Cluster 91"
Cohesion: 0.40
Nodes (5): default, description, title, type, illegalCharReplacement

### Community 92 - "Module Cluster 92"
Cohesion: 0.40
Nodes (5): default, description, title, type, keywordFilterEnabled

### Community 93 - "Module Cluster 93"
Cohesion: 0.40
Nodes (5): default, description, title, type, lastfmImportPlaycount

### Community 94 - "Module Cluster 94"
Cohesion: 0.40
Nodes (5): default, description, title, type, lastfmScrobble

### Community 95 - "Module Cluster 95"
Cohesion: 0.40
Nodes (5): default, description, title, type, lidarrForceSearchIncomplete

### Community 96 - "Module Cluster 96"
Cohesion: 0.40
Nodes (5): default, description, title, type, logWebhookUrl

### Community 97 - "Module Cluster 97"
Cohesion: 0.40
Nodes (5): default, description, title, type, mysqlHost

### Community 98 - "Module Cluster 98"
Cohesion: 0.40
Nodes (5): default, description, title, type, mysqlName

### Community 99 - "Module Cluster 99"
Cohesion: 0.40
Nodes (5): default, description, title, type, mysqlUser

### Community 100 - "Module Cluster 100"
Cohesion: 0.40
Nodes (5): default, description, title, type, nestBucketsUnderVarious

### Community 101 - "Module Cluster 101"
Cohesion: 0.40
Nodes (5): default, description, title, type, overwriteArt

### Community 102 - "Module Cluster 102"
Cohesion: 0.40
Nodes (5): default, description, title, type, overwriteExistingTags

### Community 103 - "Module Cluster 103"
Cohesion: 0.40
Nodes (5): default, description, title, type, persistenceUrl

### Community 104 - "Module Cluster 104"
Cohesion: 0.40
Nodes (5): default, description, title, type, playbackStatsEnabled

### Community 105 - "Module Cluster 105"
Cohesion: 0.40
Nodes (5): default, description, title, type, preserveRecordingType

### Community 106 - "Module Cluster 106"
Cohesion: 0.40
Nodes (5): pruneEmptyDirs, default, description, title, type

### Community 107 - "Module Cluster 107"
Cohesion: 0.40
Nodes (5): readNfo, default, description, title, type

### Community 108 - "Module Cluster 108"
Cohesion: 0.40
Nodes (5): renameSidecars, default, description, title, type

### Community 109 - "Module Cluster 109"
Cohesion: 0.40
Nodes (5): rollbackRunId, default, description, title, type

### Community 110 - "Module Cluster 110"
Cohesion: 0.40
Nodes (5): runOnlyWhenIdle, default, description, title, type

### Community 111 - "Module Cluster 111"
Cohesion: 0.40
Nodes (5): runOnStartup, default, description, title, type

### Community 112 - "Module Cluster 112"
Cohesion: 0.40
Nodes (5): scheduleCron, default, description, title, type

### Community 113 - "Module Cluster 113"
Cohesion: 0.40
Nodes (5): singlesEnabled, default, description, title, type

### Community 114 - "Module Cluster 114"
Cohesion: 0.40
Nodes (5): singlesFolder, default, description, title, type

### Community 115 - "Module Cluster 115"
Cohesion: 0.40
Nodes (5): singlesUnderArtist, default, description, title, type

### Community 116 - "Module Cluster 116"
Cohesion: 0.40
Nodes (5): skipHiddenFiles, default, description, title, type

### Community 117 - "Module Cluster 117"
Cohesion: 0.40
Nodes (5): skipUnverified, default, description, title, type

### Community 118 - "Module Cluster 118"
Cohesion: 0.40
Nodes (5): soundtrackFolder, default, description, title, type

### Community 119 - "Module Cluster 119"
Cohesion: 0.40
Nodes (5): starTallyEnabled, default, description, title, type

### Community 120 - "Module Cluster 120"
Cohesion: 0.40
Nodes (5): useLidarrNamingSchema, default, description, title, type

### Community 121 - "Module Cluster 121"
Cohesion: 0.40
Nodes (5): variousFolder, default, description, title, type

### Community 122 - "Module Cluster 122"
Cohesion: 0.40
Nodes (5): verifyIdentity, default, description, title, type

### Community 123 - "Module Cluster 123"
Cohesion: 0.40
Nodes (5): writeCoverJpg, default, description, title, type

### Community 124 - "Module Cluster 124"
Cohesion: 0.40
Nodes (5): writeNfo, default, description, title, type

### Community 125 - "Module Cluster 125"
Cohesion: 0.40
Nodes (5): writePlaycount, default, description, title, type

### Community 126 - "Module Cluster 126"
Cohesion: 0.40
Nodes (5): writeTagsForTracked, default, description, title, type

### Community 127 - "Module Cluster 127"
Cohesion: 0.60
Nodes (5): nd-organizer, nd-pdk, nd-pdk-capabilities, nd-pdk-host, nd-pdk-types

### Community 128 - "Module Cluster 128"
Cohesion: 0.40
Nodes (5): _fetch_json(), playlist_html(), radio_html(), Internet radio panel: existing stations + AJAX search/add/remove/rename. All…, Smart Playlist panel: list existing + create new / deploy presets.

### Community 129 - "Module Cluster 129"
Cohesion: 0.50
Nodes (4): description, title, type, acoustidApiKey

### Community 130 - "Module Cluster 130"
Cohesion: 0.50
Nodes (4): description, title, type, lastfmApiKey

### Community 131 - "Module Cluster 131"
Cohesion: 0.50
Nodes (4): description, title, type, lastfmUser

### Community 132 - "Module Cluster 132"
Cohesion: 0.50
Nodes (4): description, title, type, lidarrApiKey

### Community 133 - "Module Cluster 133"
Cohesion: 0.50
Nodes (4): description, title, type, musicbrainzToken

### Community 135 - "Module Cluster 135"
Cohesion: 0.50
Nodes (3): purge_missing(), Result, String

### Community 136 - "Module Cluster 136"
Cohesion: 0.50
Nodes (4): _action_chips(), actions_html(), Small stage chips derived from an album's actions (moves / nfo / art / lyrics /…, THE transparency view: every distinct album plan ever reported, newest first,…

## Knowledge Gaps
- **420 isolated node(s):** `nd-organizer`, `name`, `author`, `version`, `description` (+415 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **1 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Config` connect `Configuration & Modes` to `WASM Lifecycle & Scheduling`, `Album Organization & Grouping`, `Star Ratings & Playback Stats`, `Favorites & Last.fm Sync`, `Logging & Store Backend`, `Module Cluster 23`?**
  _High betweenness centrality (0.201) - this node is a cross-community bridge._
- **Why does `lidarr_send()` connect `Configuration & Modes` to `Module Cluster 28`?**
  _High betweenness centrality (0.132) - this node is a cross-community bridge._
- **Why does `HTTPResponse` connect `Module Cluster 28` to `Configuration & Modes`?**
  _High betweenness centrality (0.131) - this node is a cross-community bridge._
- **What connects `nd-organizer`, `name`, `author` to the rest of the system?**
  _420 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Configuration & Modes` be split into smaller, more focused modules?**
  _Cohesion score 0.05041087231352718 - nodes in this community are weakly interconnected._
- **Should `WASM Lifecycle & Scheduling` be split into smaller, more focused modules?**
  _Cohesion score 0.05112347969490827 - nodes in this community are weakly interconnected._
- **Should `Album Organization & Grouping` be split into smaller, more focused modules?**
  _Cohesion score 0.07562479714378449 - nodes in this community are weakly interconnected._