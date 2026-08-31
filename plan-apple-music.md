# Plan: Complete Metadata Source System with Fallback Chains

## Goal
Every metadata type gets a dropdown of all sources that can provide it. User selects primary for each type. Automatic fallback when primary fails. Identity confirmed before metadata enrichment.

## Key Decisions
1. **MusicBrainz = default genre provider** — free API, no extra calls needed
2. **NFO = last fallback** — unless user selects as primary
3. **Lyrics: LRCLIB default** — no "disabled" option, empty = disabled
4. **Identity first** — only fetch metadata AFTER fingerprinting confirms track/album/artist
5. **Write only if enabled** — NFO gated by `writeNfo`, tags gated by per-type toggles

## Pipeline Order (critical)
```
1. scan_step     → index files, read embedded tags
2. group_step    → AcoustID fingerprint → confirm identity (MBID, release, artist)
3. plan_step     → move files, then enrich:
   a. MusicBrainz classification (if primarySource=musicbrainz)
   b. Auto-tag from MB (fill missing title/artist/MBIDs)
   c. ReplayGain
   d. Artwork (fallback chain) → write if embedArtwork/writeCoverJpg enabled
   e. Genre (fallback chain) → write if genreSource configured
   f. Lyrics (fallback chain) → write if lyricsSource configured
   g. BPM/Key/Mood (AudioMuse → Essentia fallback) → write if writeAcousticTags enabled
   h. Apple Music enrichment (fill remaining gaps)
   i. Write NFO files → ONLY if writeNfo enabled
   j. Write track tags → ONLY if per-type toggles enabled
   k. Lidarr refresh
```

## Dropdowns

### 1. Primary Metadata Source (`primarySource`)
| Value | Title | What it fills |
|-------|-------|---------------|
| `musicbrainz` | MusicBrainz (default) | Title, artist, album, year, release type, MBIDs, track list |
| `applemusic` | Apple Music / iTunes | Title, artist, album, year |
| `lidarr` | Lidarr (tracked artists) | Title, artist, album |
| `nfo` | NFO files (offline) | Title, artist, album, year, genre, styles, moods |

**Fallback**: primary → next configured → NFO (always last unless selected)

### 2. Artwork Source (`artworkSource`)
| Value | Title | Needs |
|-------|-------|-------|
| `coverartarchive` | Cover Art Archive (default) | MBID |
| `applemusic` | Apple Music / iTunes | Artist+album name |
| `theaudiodb` | TheAudioDB | API key |
| `embedded` | Existing embedded art | Nothing |

**Fallback**: selected → next configured → embedded (always last)

### 3. Genre Source (`genreSource`)
| Value | Title | Needs |
|-------|-------|-------|
| `musicbrainz` | MusicBrainz (default) | Nothing (free API) |
| `discogs` | Discogs | API token |
| `essentia` | Essentia (ML analysis) | Sidecar URL |
| `nfo` | NFO files (offline) | Existing NFO |

**Fallback**: selected → next configured → NFO (always last unless selected)

### 4. Lyrics Source (`lyricsSource`)
| Value | Title | Format |
|-------|-------|--------|
| `lrclib` | LRCLIB (default) | Synced (.lrc) or plain (.txt) via `lyricsFormat` |
| `genius` | Genius | Plain (.txt) + annotations |

**Fallback**: selected → next configured. Empty = disabled.

### 5. BPM/Key/Mood (automatic)
| Source | Provides | Config gate |
|--------|----------|-------------|
| AudioMuse-AI | BPM, key, mood, energy | `audiomuseUrl` + `writeAcousticTags` |
| Essentia | Mood (fallback) | `essentiaUrl` + `genreFrom="essentia"` |

## Changes

### 1. New `src/apple_music.rs`
- `resolve_artist_id(name, countries)` → iTunes Search API
- `fetch_album_artwork(artist, album, countries)` → iTunes Lookup API (rewrite 1500x1500)
- `fetch_artist_image(name, countries)` → Apple Music web scraping
- `fetch_artist_bio(name, countries)` → Apple Music web scraping
- `detect_country()` → ND_LOCALE/ND_DEFAULT_LANGUAGE env vars, fallback "us"
- Circuit breaker, throttle, KVStore caching, multi-country fallback

### 2. Enhance `src/theaudiodb.rs`
- Add `fetch_album_artwork(key, artist, album) -> Option<Vec<u8>>`
- Add genre parsing (extract `strGenre` from API response)

### 3. Enhance `src/discogs.rs`
- Add `fetch_genres(token, artist, album) -> Option<Vec<String>>`

### 4. Enhance `src/musicbrainz.rs`
- Add `fetch_genres(release_mbid) -> Option<Vec<String>>`

### 5. Config (`src/config.rs`)
- Rename `artwork_priority` → `artwork_source`
- Rename `genre_from` → `genre_source`
- Replace `download_lyrics` bool → `lyrics_source` string
- Keep `lyrics_format` (only applies to LRCLIB)
- Add `apple_music_countries`, `apple_music_cache_ttl`
- Rename `PrimarySource::Itunes` → `PrimarySource::AppleMusic`
- Add `PrimarySource::Nfo` variant

### 6. Fallback chains (`src/scan.rs`)
- `fetch_artwork_with_fallback()` — tries sources in order, returns (bytes, source_name)
- `fetch_genre_with_fallback()` — tries sources in order, returns (genres, source_name)
- `fetch_lyrics_with_fallback()` — tries sources in order, returns (lyrics, source_name)

### 7. Module registration (`src/lib.rs`)
- Add `mod apple_music;`
- Add health check card for Apple Music

### 8. UI (`manifest.json`)
- Rename dropdowns, add options, add Apple Music fields, UI schema

### 9. Docs
- `AGENTS.md` — update metadata sources
- `README.md` — complete metadata matrix with ALL sources including sidecars

## Files

| File | Change |
|------|--------|
| `src/apple_music.rs` | **New** — provider module |
| `src/lib.rs` | `mod apple_music;`, health check |
| `src/config.rs` | Rename fields, add Apple Music, rename enum |
| `src/scan.rs` | Fallback chains + enrichment pipeline |
| `src/artwork.rs` | `fetch_apple_music()` |
| `src/theaudiodb.rs` | Album artwork download, genre parsing |
| `src/discogs.rs` | Genre enrichment |
| `src/musicbrainz.rs` | Genre fetch |
| `manifest.json` | All dropdowns, fields, UI |
| `AGENTS.md` | Update metadata sources |
| `README.md` | Complete matrix with sidecars |

## Implementation Order
1. Create `src/apple_music.rs`
2. Update `src/config.rs`
3. Update `src/artwork.rs`
4. Enhance `src/theaudiodb.rs`
5. Enhance `src/discogs.rs`
6. Enhance `src/musicbrainz.rs`
7. Update `src/scan.rs` (fallback chains + pipeline)
8. Register in `src/lib.rs` + health check
9. Update `manifest.json`
10. Update docs
11. Build, test, clippy
