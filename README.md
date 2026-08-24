# nd-organizer

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.75+-orange.svg)](https://www.rust-lang.org/)
[![Navidrome](https://img.shields.io/badge/Navidrome-Plugin-green.svg)](https://www.navidrome.org/)
[![Docker](https://img.shields.io/badge/Docker-Sidecars-blue.svg)](docker-compose.yml)
[![GitHub release](https://img.shields.io/github/v/release/lunatixz/nd-organizer)](https://github.com/lunatixz/nd-organizer/releases)

A [Navidrome](https://www.navidrome.org/) plugin (Rust → WebAssembly, packaged as
`.ndp`) that organizes your music library — slowly and accurately:

- **Full-library scan** reads every file's tags into a persistent index, then
  groups files into albums by their **metadata** (MusicBrainz IDs / ISRC), not
  by their folders — so scattered songs that belong to one album are recognized
  as one album instead of a pile of "singles".
- **Identity verification before anything moves**: files with no reliable ID
  (MBID/ISRC) are fingerprinted via an **AcoustID sidecar** (Docker) so a song
  is only paired to an album when it's actually identified. Unverified files
  are routed to the artist's Singles folder.
- **Classifies albums** (Soundtrack > Various Artist > Singles > Normal) and
  renames folders/files to your schemas:
  - Soundtracks → `Various Artist/Sound Tracks/{album} ({year})`
  - Compilations → `Various Artist/{album} ({year})`
  - Singles / incomplete albums → `{albumArtist}/Singles/{title}`
  - Everything else → `{albumArtist}/{album} ({year})`
- **Preserves recording source**: live/bootleg tracks get `(Live)`/`(Bootleg)`
  suffixes so they never collide with the studio release.
- **Duplicates**: two files that are the same album, artist, title, track **and
  identical size** are confirmed duplicates; the loser is moved to the artist's
  `Singles` folder (filename disambiguated, e.g. `Title (from Album).flac`, so it
  never overwrites existing singles).
- **Filler tracks**: intro/outro/interlude tracks (keyword-configurable) are
  filtered out of **every media response** by the **Navidrome filter proxy** (except explicit user searches)
  (an explicit search still returns them). **Albums stay whole and files are
  never moved.**
- **Skip-content limiter**: skip-heavy (low-star) tracks never dominate your
  queues — choose `exclude` / `third` / `lessThanHalf` (default) / `half` /
  `none` for how much may appear. No files moved.
- **Favorites sync**: Navidrome (the hub, Subsonic stars) <-> Last.fm loved
  tracks; any Subsonic-compatible client participates automatically through
  server-side stars.
- **Playback stats**: tracks plays vs skips and builds an
  **"nd-organizer: Top Picks"** Navidrome playlist of the highest-weight songs.
  Weights + skip-heavy flags are published to the **filter proxy**, which
  re-sorts returned song lists so skipped tracks sink. A full play forgives a
  skip, and only net-negative tracks (skipped more than ever played fully, past
  a user-set cap) are removed — songs you occasionally skip stay hearable, and
  **no files are moved**.
- **Star rating + playcount (0–5.0)**: every listen is classified by its playback
  percentage into a **full star** (≥ `starFullPlayPercent`, default 85%), a
  **half star** (≥ `starHalfPlayPercent`, default 55%), or a **skip penalty**
  (below it). Full listens earn **+1.0★ and +1 playcount**; half listens **+0.5★**;
  skips are a **−0.5★ penalty** that a later full listen **forgives**. The rating
  is stored in the plugin's own storage (durable via `persistenceBackend = mysql`)
  and **published to Navidrome's native rating** (`setRating`) once a track has
  ≥ `starMinSamples` listens — the 0.5 precision lives in the plugin DB. Full
   plays can optionally be **scrobbled to Last.fm** (`lastfmScrobble`) and/or
   **ListenBrainz** (`listenbrainzScrobble`); the baseline can be
   **imported from Last.fm** (`lastfmImportPlaycount`).
   **No audio-file tags are written.**
- **Refreshes metadata + artwork** from MusicBrainz, Cover Art Archive, iTunes
  and Last.fm; reads/writes Kodi-style `album.nfo`/`artist.nfo` sidecars.
- **Integrates with Lidarr** (metadata/classification source, optional Lidarr
  naming schema, force-search for incomplete **monitored** albums) and
  **AudioMuse-AI** (acoustic BPM/key/mood tags, re-sync after renames).
- **Player-friendly**: runs only when nothing is playing (`runOnlyWhenIdle`), in
  small background batches, and **triggers a Navidrome rescan after every album
  it moves** so no song ever goes offline.
- **Safety**: dry-run default, full change history with **rollback**
  (`rollbackRunId`, auto-pruned after `rollbackRetentionDays`), metadata-only
  backups, per-run reports + a live status feed with album plans and a rollback
  callout.

## How it works (pipeline)

1. **Scan** — a chunked, resumable walk reads tags (`filesPerScanTask` per task)
   into the plugin's KVStore. Re-scans are incremental (mtime-cached), so it's
   slow-and-accurate, not repeated.
2. **Verify** — files with a MusicBrainz ID or ISRC are verified. Files without
   one are fingerprinted via the AcoustID sidecar to obtain an album MBID.
   Files that still can't be identified are routed to Singles (reported).
3. **Group** — verified files are grouped into albums by album MBID, or by
   album artist + album + year. Files with no usable tags fall back to
   **filename parsing** (`parseFilenames`), and files sharing an audio
   fingerprint across the library are reported as **possible duplicates**
   (`detectDuplicates`).
4. **Plan** — each album group is planned in batches (`albumsPerTask`): target
   folder/file names from your schemas, duplicate detection, plus
   **auto-tagging** missing fields from the MusicBrainz release tracklist
   (`autoTagFromMB`).
5. **Apply** (only in `mode: apply`) — moves files + sidecars, handles
   duplicates, records every change for rollback, and triggers a Navidrome
   rescan per album. Tag/enrichment passes also run here: **ReplayGain** loudness
   tags via the acoustid sidecar (`writeReplayGain` + `replayGainMode` /
   `replayGainReference`), artwork, lyrics, acoustic tags, `album.nfo` /
   `artist.nfo`, and the optional no-audio-folder cleanup
   (`cleanupNoAudioFolders`).

**Safety model:** `mode: dryRun` is the default — it plans and reports but writes
nothing. Review the report/status, then switch to `apply`. If a required
metadata source (AcoustID / MusicBrainz / Lidarr) is unreachable, the run skips
and retries later instead of acting on degraded data (`metaGateEnabled`).

## Star rating & playcount (0–5.0)

A universal 5-star rating derived from how you actually listen, at half-star
granularity:

| Listen ends at | Playcount | Star credit |
|---|---|---|
| ≥ `starFullPlayPercent` (85%) | **+1** | **+1.0 ★** |
| `starHalfPlayPercent`–`starFullPlayPercent` (55–84%) | — | **+0.5 ★** |
| `starIgnorePercent`–`starHalfPlayPercent` (5–54%) | — | **−0.5 ★** (penalty) |
| < `starIgnorePercent` (5%) | — | **Ignored** (no penalty) |

- **Playcount is full listens only.** A skip never increments it, so it acts as a
  pure penalty against the rating.
- **A full listen forgives one prior skip** — sustain full listening and a
  rating recovers.
- Rating is **capped at 0–5.0** and rounded to the nearest **0.5**.
- **Loved** = rating ≥ `lovedThresholdStars` (default 3). Loved tracks are
  starred in Navidrome and tagged with `LOVED` status; beloved tracks ≥ 3 stars
  start at that baseline when first observed.
- **Album ratings** average the album's track star ratings. External ratings
  (NFO / MusicBrainz / Lidarr) take priority over the calculated average.
- **Initial rating** seeds from the strongest signal: Lidarr track/album rating
  (directly used), or Last.fm playcount (higher playcount → more stars, up to
  4.5 ceiling) + Last.fm loved (floors at 3 stars).
- **Published to Navidrome** (`setRating` + `star`/`unstar`) once a track has ≥
  `starMinSamples` observed listens. Star/unstar mirrors the loved threshold.
- Stored in the plugin's KVStore, keyed by **file path + filename** — so it
  survives restarts, and if you enable `persistenceBackend = mysql` it lives in
  your own durable database.
- **Follows file renames**: when the organizer moves a file the tally is migrated
  to the new path, and orphaned tallies are pruned automatically.
- **Scrobble** (both opt-in): `lastfmScrobble` scrobbles full listens to
  **Last.fm** (`track.scrobble`); `listenbrainzScrobble` scrobbles to
  **ListenBrainz** (`submit-listens`). Keep each OFF if Navidrome's own
  scrobbler already covers that service (double-counts otherwise).
- **Never writes to the audio file** — the rating/playcount live in the plugin
  DB and Navidrome only.
- The **dry-run report** previews each tracked file's current stars + playcount,
  so you see exactly what an apply run would publish.

## Rating sync (multi-source)

Ratings and loved/favorite status stay in sync across all configured sources:

| Source | Read | Write | Config |
|--------|------|-------|--------|
| **Navidrome** (Subsonic) | Stars + setRating | setRating + star/unstar | always (hub) |
| **Last.fm** | Loved tracks + playcount | track.love | `favoritesSyncLastfm` |
| **Lidarr** | Track + album ratings | Track + album ratings | `ratingSyncWriteToLidarr` |
| **Plugin DB** | Star tally (canonical) | — | `starTallyEnabled` |

**How it works:**
- The plugin DB is the **canonical source** — it has the most granular data
  (play/skip ratio, half-star precision).
- **Initial seed**: when a track is first observed, the highest external rating
  wins (Lidarr > Last.fm > Navidrome playcount mapping).
- **Ongoing**: the plugin's computed rating propagates outward on every stats
  pass — to Navidrome (`setRating` + `star`/`unstar`) and optionally to
  Lidarr (track + album ratings).
- **Pull from Navidrome**: `ratingSyncPullFromNavidrome` imports ratings set
  manually in Navidrome's UI into the plugin DB.
- **Loved = OR**: if any source says loved, mark as loved. Unlove is only
  propagated via Navidrome unstar (additive-only for Last.fm).
- **Conflict resolution**: higher rating wins on initial import; after that,
  the plugin DB value propagates outward.

## Docker setup (docker-compose)

The plugin itself is a `.ndp` file in Navidrome's plugins folder. Optional
**sidecar images** run as containers:

| Image | Purpose |
|---|---|
| `ghcr.io/v1ck3s/octo-fiesta` | Missing-track proxy (third-party, GPL-3.0) — when a requested song isn't in the library, fetches it from the configured provider and streams it. Supports SquidWTF (free, no creds), Deezer, Qobuz, Yandex. |
| `ghcr.io/lunatixz/nd-organizer/acoustid:latest` | Fingerprints songs (AcoustID) so unverified files can be paired to their album, and computes ReplayGain loudness tags (ffmpeg). |
| `ghcr.io/lunatixz/nd-organizer/webhook:latest` | A web dashboard showing status + reports (auto-refreshing). |
| `ghcr.io/lunatixz/nd-organizer/proxy:latest` | Subsonic filtering proxy — sits in front of Navidrome; drops filler-keyword tracks from every media response (except explicit user searches), limits skip-heavy content in queued lists, and re-sorts by weight — without touching files. |
| `ghcr.io/lunatixz/nd-organizer/mysql:latest` | Optional MySQL bridge — executes the plugin's kvstore operations against your MySQL/MariaDB when `persistenceBackend = mysql`. |
| `ghcr.io/lunatixz/nd-organizer/radio:latest` | Internet radio sidecar (based on WB2024/Add-Navidrome-Radios) — search the Radio-Browser community DB and add stations straight into Navidrome's `radio` table (no restart). The webhook dashboard has a Radio panel. |
| `ghcr.io/neptunehub/audiomuse-ai:latest` | Optional sonic-analysis server (third-party, AGPL-3.0) — powers acoustic BPM/key/mood tags and re-sync after renames. Runs as postgres + flask (`audiomuse-ai-flask-app`, `:8000`) + worker. **Commented out** in the compose. |

The compose files reference the published GHCR images — `docker compose up`
pulls them (no local build, no build context needed). Tags: `:latest`, `:main`,
and `vX.Y.Z` semver tags per release.

Here is the **complete `docker-compose.yml`** — Navidrome plus all seven sidecars
(octo-fiesta, acoustid, webhook, filter proxy, mysql, radio, essentia) on one shared network.
Copy it to your NAS, fill in the paths, then run `docker compose up -d`:

```yaml
# nd-organizer full stack - Navidrome plus all seven sidecars, one command:
#   docker compose up -d
#
# Services:
#   navidrome                  (4533)  the music server (plugins enabled)
#   octo-fiesta                (4535)  missing-track proxy (multi-provider)
#   nd-organizer-acoustid (8097)  fingerprint + ReplayGain sidecar (fpcalc + ffmpeg)
#   nd-organizer-webhook  (8099)  dashboard + log/status receiver
#   nd-organizer-proxy    (4534)  Subsonic filter proxy
#   nd-organizer-mysql    (8098)  optional MySQL KV bridge for the plugin's state
#   nd-organizer-radio    (8100)  internet radio sidecar (Radio-Browser -> Navidrome)
#   nd-organizer-essentia (8101)  genre/mood ML analysis (Essentia + Discogs-400)
#
# Streaming chain / player setup (server type: Subsonic/OpenSubsonic; use your
# normal Navidrome user/password - credentials pass through unchanged):
#   player -> octo-fiesta (4535) -> nd-organizer-proxy (4534) -> navidrome (4533)  [full stack]
#   player -> nd-organizer-proxy (4534) -> navidrome (4533)                        [filtering only]
#   player -> navidrome (4533)                                                     [no proxy]
# Point the player at http://<your-nas>:4535/rest/ to get queue filtering AND
# missing-track fallback. octo-fiesta is transparent for songs Navidrome already
# has; only tracks NOT in the library are fetched from the configured provider.
#
# Providers (octo-fiesta supports all of these; fill credentials in a .env
# file next to this compose and pick one via MUSIC_SERVICE):
#   SquidWTF - no credentials (free, proxies Qobuz/Tidal/Amazon/Deemix)
#   Deezer   - DEEZER_ARL (see octo-fiesta wiki "Getting-Deezer-Credentials")
#   Qobuz    - QOBUZ_USER_AUTH_TOKEN + QOBUZ_USER_ID (paid account)
#   Yandex   - YANDEX_OAUTH_TOKEN (see wiki "Getting-Yandex-Credentials")
#
# Prerequisites:
#   - a user-defined Docker network named `stack_network`:
#         docker network create stack_network
#   - if you ALREADY run Navidrome separately, remove the `navidrome` service
#     below and just connect your existing container to the network instead:
#         docker network connect stack_network navidrome
#   - acoustid: mirror EVERY library mount at the SAME guest paths Navidrome uses.
#   - mysql: optional - only if you set persistenceBackend=mysql in the plugin.
#
# Plugin settings to match:
#   logWebhookUrl = http://nd-organizer-webhook:8099
#   acoustidUrl   = http://nd-organizer-acoustid:8097
#   filterUrl     = http://nd-organizer-proxy:4534
#   persistenceUrl= http://nd-organizer-mysql:8098   (if using mysql backend)
#   octoFiestaUrl = http://octo-fiesta:8080         (or your NAS:4535; webhook health)

services:
  navidrome:
    image: deluan/navidrome:latest
    container_name: navidrome
    restart: unless-stopped
    ports:
      - "4533:4533"
    environment:
      - ND_PLUGINS_ENABLED=true
      - ND_PLUGINS_AUTORELOAD=true
      # Default library. Additional libraries are added in the UI
      # (Settings -> Libraries) pointing at the mounted guest paths below.
      - ND_MUSICFOLDER=/music
      # Import playlists (.m3u) from this folder, relative to ND_MUSICFOLDER.
      # Colon-separated for multiple folders/globs. Empty = scan the whole library.
      - ND_PLAYLISTSPATH=playlists
      # Optional: agents the plugin can power for the UI (see README/agents):
      # - ND_AGENTS=nd-organizer
    volumes:
      - ./navidrome-data:/data
      - /path/to/music:/music:rw        # library 1  (path = /music)
      - /path/to/unsorted:/unsorted:rw  # library 2  (path = /unsorted)
      # add more - /host/path:/guest:rw lines for every library
    networks:
      - stack_network

  # ===== AudioMuse-AI (OPTIONAL - sonic analysis / metadata server) =====
  # Uncomment these three services to deploy AudioMuse-AI
  # (ghcr.io/neptunehub/audiomuse-ai, AGPL-3.0) on this stack, then set the
  # plugin's audiomuseUrl = http://audiomuse-ai-flask-app:8000 and run its setup
  # wizard once (admin user + connect your Navidrome server).
  # Container names match AudioMuse-AI's own deployment example.
  # Requires ~8 GB RAM and a 4-core CPU with AVX2 (see AudioMuse-AI docs);
  # a heavy service - that's why this block is commented out by default.
  # audiomuse-postgres:
  #   image: postgres:15-alpine
  #   container_name: audiomuse-postgres
  #   restart: unless-stopped
  #   environment:
  #     TZ: UTC
  #     POSTGRES_USER: audiomuse
  #     POSTGRES_PASSWORD: changeme
  #     POSTGRES_DB: audiomusedb
  #   volumes:
  #     - audiomuse-postgres-data:/var/lib/postgresql/data
  #   networks:
  #     - stack_network
  #
  # audiomuse-ai-flask:
  #   image: ghcr.io/neptunehub/audiomuse-ai:latest
  #   container_name: audiomuse-ai-flask-app
  #   restart: unless-stopped
  #   ports:
  #     - "8000:8000"
  #   environment:
  #     SERVICE_TYPE: flask
  #     TZ: UTC
  #     POSTGRES_USER: audiomuse
  #     POSTGRES_PASSWORD: changeme
  #     POSTGRES_DB: audiomusedb
  #     POSTGRES_HOST: audiomuse-postgres
  #     POSTGRES_PORT: "5432"
  #     TEMP_DIR: /app/temp_audio
  #   volumes:
  #     - audiomuse-temp-flask:/app/temp_audio
  #   depends_on:
  #     - audiomuse-postgres
  #   networks:
  #     - stack_network
  #
  # audiomuse-ai-worker:
  #   image: ghcr.io/neptunehub/audiomuse-ai:latest
  #   container_name: audiomuse-ai-worker-instance
  #   restart: unless-stopped
  #   environment:
  #     SERVICE_TYPE: worker
  #     TZ: UTC
  #     POSTGRES_USER: audiomuse
  #     POSTGRES_PASSWORD: changeme
  #     POSTGRES_DB: audiomusedb
  #     POSTGRES_HOST: audiomuse-postgres
  #     POSTGRES_PORT: "5432"
  #     TEMP_DIR: /app/temp_audio
  #   volumes:
  #     - audiomuse-temp-worker:/app/temp_audio
  #   depends_on:
  #     - audiomuse-postgres
  #   networks:
  #     - stack_network

  octo-fiesta:
    image: ghcr.io/v1ck3s/octo-fiesta
    container_name: octo-fiesta
    restart: unless-stopped
    ports:
      - "4535:8080"
    environment:
      - ASPNETCORE_ENVIRONMENT=Production
      - ASPNETCORE_URLS=http://+:8080
      - Library__DownloadPath=/app/downloads
      # Upstream = our filter proxy (keeps keyword/skip filtering), which
      # forwards to Navidrome. Point directly at http://navidrome:4533 to skip
      # the filter layer for this path.
      - Subsonic__Url=http://nd-organizer-proxy:4534
      # Music service: SquidWTF (no creds) | Deezer | Qobuz | Yandex
      - Subsonic__MusicService=${MUSIC_SERVICE:-SquidWTF}
      # Cache = stream missing tracks without writing to the library.
      # Set to Permanent + mount DOWNLOAD_PATH to save them into the library.
      - Subsonic__StorageMode=${STORAGE_MODE:-Cache}
      - Subsonic__CacheDurationHours=${CACHE_DURATION_HOURS:-1}
      - Subsonic__EnableExternalPlaylists=${ENABLE_EXTERNAL_PLAYLISTS:-true}
      - Subsonic__PlaylistsDirectory=${PLAYLISTS_DIRECTORY:-playlists}
      - Subsonic__ExplicitFilter=${EXPLICIT_FILTER:-All}
      - Subsonic__DownloadMode=${DOWNLOAD_MODE:-Track}
      - Subsonic__AutoUpgradeQuality=${AUTO_UPGRADE_QUALITY:-false}
      - Subsonic__DisableLibraryScan=${DISABLE_LIBRARY_SCAN:-false}
      # NOTE: the folder template default ({artist}/{album}/{track} - {title})
      # lives in .env.example - braces inside a ${VAR:-default} break compose's
      # interpolation, so FOLDER_TEMPLATE must come from the .env file.
      - Subsonic__FolderTemplate=${FOLDER_TEMPLATE}
      # Admin creds only needed for Permanent-mode library registration.
      - Subsonic__AdminUsername=${SUBSONIC_ADMIN_USERNAME:-}
      - Subsonic__AdminPassword=${SUBSONIC_ADMIN_PASSWORD:-}

      # ===== SQUIDWTF (free, no credentials) =====
      # Backend: Qobuz | Tidal | AmazonMusic | Deemix
      - SquidWTF__Source=${SQUIDWTF_SOURCE:-Qobuz}
      # Quality: Qobuz 27/7/6/5, Tidal HI_RES_LOSSLESS/LOSSLESS/HIGH/LOW,
      #          AmazonMusic FLAC_24/FLAC_16/AAC/OPUS/ATMOS, Deemix FLAC/MP3_320/MP3_128
      - SquidWTF__Quality=${SQUIDWTF_QUALITY:-6}
      - SquidWTF__InstanceTimeoutSeconds=${SQUIDWTF_INSTANCE_TIMEOUT:-5}
      # Force a specific Tidal API instance (e.g. a self-hosted hifi-api). When
      # set, the remote instances.json is NOT fetched - only this URL is used.
      - SquidWTF__Instances__0=${SQUIDWTF_INSTANCE:-}
      # Override URL of the remote instances.json (ignored if SQUIDWTF_INSTANCE
      # is set). Defaults to https://tidal-uptime.geeked.wtf/ if empty.
      - SquidWTF__InstancesUrl=${SQUIDWTF_INSTANCES_URL:-}

      # ===== DEEZER (requires DEEZER_ARL) =====
      - Deezer__Arl=${DEEZER_ARL:-}
      - Deezer__ArlFallback=${DEEZER_ARL_FALLBACK:-}
      - Deezer__Quality=${DEEZER_QUALITY:-}

      # ===== QOBUZ (requires QOBUZ_USER_AUTH_TOKEN + QOBUZ_USER_ID) =====
      - Qobuz__UserAuthToken=${QOBUZ_USER_AUTH_TOKEN:-}
      - Qobuz__UserId=${QOBUZ_USER_ID:-}
      - Qobuz__Quality=${QOBUZ_QUALITY:-}

      # ===== YANDEX (requires YANDEX_OAUTH_TOKEN) =====
      - Yandex__OAuthToken=${YANDEX_OAUTH_TOKEN:-}
      - Yandex__Quality=${YANDEX_QUALITY:-}
      - Yandex__Language=${YANDEX_LANGUAGE:-en}
      - Yandex__IncludeUnavailable=${YANDEX_INCLUDE_UNAVAILABLE_TRACKS:-false}
    networks:
      - stack_network
    # OPTIONAL - Permanent mode saves fetched tracks into a Navidrome-scanned
    # library folder (host path must match a Navidrome library mount):
    #   volumes:
    #     - ${DOWNLOAD_PATH:-/path/to/music/Octo-Fiesta}:/app/downloads
    #   environment:
    #     - Subsonic__StorageMode=Permanent

  nd-organizer-acoustid:
    image: ghcr.io/lunatixz/nd-organizer/acoustid:latest
    container_name: nd-organizer-acoustid
    restart: unless-stopped
    ports:
      - "8097:8097"
    environment:
      - WEBHOOK_URL=http://nd-organizer-webhook:8099   # heartbeat -> dashboard
    volumes:
      # Mirror every library mount from your Navidrome stack, SAME guest path.
      - /path/to/music:/music:ro
      - /path/to/unsorted:/unsorted:ro
    networks:
      - stack_network

  nd-organizer-webhook:
    image: ghcr.io/lunatixz/nd-organizer/webhook:latest
    container_name: nd-organizer-webhook
    restart: unless-stopped
    ports:
      - "8099:8099"
    volumes:
      - ./data:/data          # webhook.log persists here
      # Read-only Docker socket: lets the dashboard read octo-fiesta's logs
      # (octo exposes only the Subsonic API, no /logs endpoint).
      - /var/run/docker.sock:/var/run/docker.sock:ro
    environment:
      # octo-fiesta's URL + provider come from the plugin's Navidrome config
      # (octoFiestaUrl / octoFiestaProvider), reported in status POSTs. Only the
      # docker-log container name stays here (it's infrastructure, not a URL).
      - OCTO_FIESTA_CONTAINER=octo-fiesta
    networks:
      - stack_network
    # The dashboard pulls each sidecar's /logs by container name (same
    # network required), so this one UI shows plugin status + all sidecar logs.

  nd-organizer-proxy:
    image: ghcr.io/lunatixz/nd-organizer/proxy:latest
    container_name: nd-organizer-proxy
    restart: unless-stopped
    ports:
      - "4534:4534"
    environment:
      - NAVIDROME_URL=http://navidrome:4533
      - WEBHOOK_URL=http://nd-organizer-webhook:8099   # heartbeat -> dashboard
      # - FILTER_KEYWORDS=intro,outro,interlude
    networks:
      - stack_network

  nd-organizer-mysql:
    image: ghcr.io/lunatixz/nd-organizer/mysql:latest
    container_name: nd-organizer-mysql
    restart: unless-stopped
    ports:
      - "8098:8098"
    environment:
      - WEBHOOK_URL=http://nd-organizer-webhook:8099   # heartbeat -> dashboard
    networks:
      - stack_network

  # Internet radio sidecar (from WB2024/Add-Navidrome-Radios): search/add radio
  # stations via Radio-Browser, written straight into Navidrome's `radio` table.
  # Mount Navidrome's DATA dir (with navidrome.db) read-write at /data.
  nd-organizer-radio:
    image: ghcr.io/lunatixz/nd-organizer/radio:latest
    container_name: nd-organizer-radio
    restart: unless-stopped
    ports:
      - "8100:8100"
    environment:
      - NAVIDROME_DB=/data/navidrome.db
      - WEBHOOK_URL=http://nd-organizer-webhook:8099   # heartbeat -> dashboard
    volumes:
      - /path/to/navidrome/data:/data:rw    # MUST contain navidrome.db
    networks:
      - stack_network

  # ===== Essentia (genre/mood ML analysis, AudioMuse fallback) =====
  # When AudioMuse-AI is down, the plugin falls back to this service for
  # genre/mood analysis using Essentia ML models (Discogs-400 + MTG-Jamendo).
  # Requires ~100MB disk for models + 2-4GB RAM during analysis.
  nd-organizer-essentia:
    image: ghcr.io/lunatixz/nd-organizer/essentia:latest
    container_name: nd-organizer-essentia
    restart: unless-stopped
    ports:
      - "8101:8101"
    environment:
      - WEBHOOK_URL=http://nd-organizer-webhook:8099
    volumes:
      - /path/to/music:/music:ro
    networks:
      - stack_network

networks:
  stack_network:
    external: true

# Uncomment the volumes below (with the AudioMuse-AI services above) to persist
# AudioMuse-AI's PostgreSQL data and worker temp files:
# volumes:
#   audiomuse-postgres-data:
#   audiomuse-temp-flask:
#   audiomuse-temp-worker:
```

```bash
docker network create stack_network                 # once
docker network connect stack_network navidrome      # Navidrome must be on it
docker compose up -d                                # deploy everything
```

To deploy just one service, `docker compose up -d <name>` (the per-service files
in `acoustid/`, `webhook/`, `proxy/`, `mysql/`, `radio/` still work independently).
The `.env` file next to this compose supplies the `${VAR}` values — start from
the bundled **`.env.example`** (`copy .env.example .env`), which documents every
octo-fiesta option and pre-fills `FOLDER_TEMPLATE` (it must live in `.env`, not
inline in the compose — braces inside `${VAR:-default}` break interpolation).

### Shared network

One user-defined network lets every container reach the others **by container
name** (`navidrome`, `octo-fiesta`, `nd-organizer-proxy`, ...). Create it once
before `docker compose up`:

```bash
docker network create stack_network
```

The compose above deploys Navidrome too. If you already run Navidrome with its
own compose, **remove the `navidrome` service** from the file and connect your
existing container to the network instead:

```bash
docker network connect stack_network navidrome
```

### Navidrome

- Mount **each library** read-write (`:rw`) — the plugin renames/tags files. The
  guest path (`/music`, `/unsorted`, ...) becomes the library's path in Navidrome.
- `ND_MUSICFOLDER=/music` sets the **default** library. Add more libraries in the
  Navidrome UI (**Settings → Libraries**) pointing at the other mounted guest
  paths — multi-library is UI-configured, not env-driven.
- `ND_PLAYLISTSPATH=playlists` imports `.m3u` playlists from `/music/playlists`
  (relative to `ND_MUSICFOLDER`; colon-separated for several folders/globs).
  Leave empty to import from anywhere in the library.
- Copy `nd-organizer.ndp` into `./navidrome-data/plugins/`.
- Set Navidrome's **BaseUrl** to your NAS address (e.g. `http://<your-nas>:4533`).
  The plugin uses it to reach host-published sidecar ports (e.g. AudioMuse-AI's
  `8000`) when a configured container IP isn't routable from inside Navidrome.

### Octo-Fiesta (missing-track playback)

[Octo-Fiesta](https://github.com/V1ck3s/octo-fiesta) (GPL-3.0) is a third-party
Subsonic proxy that serves **songs your library doesn't have** from a streaming
provider. It supports **SquidWTF (free, no credentials)**, Deezer, Qobuz and
Yandex:

- **Streaming chain** (all on `stack_network`):
  `Subsonic client → octo-fiesta (4535) → nd-organizer-proxy (4534) → navidrome (4533)`.
  Songs Navidrome already has pass through transparently; only missing tracks
  are fetched on demand.
- **Provider** = `MUSIC_SERVICE` (default `SquidWTF` — zero credentials). For
  Deezer/Qobuz/Yandex, put the tokens in `.env` (copy the bundled
  `.env.example`) and set `MUSIC_SERVICE`:
  - `DEEZER_ARL` — [getting Deezer credentials](https://github.com/V1ck3s/octo-fiesta/wiki/Getting-Deezer-Credentials-(ARL-Token))
  - `QOBUZ_USER_AUTH_TOKEN` + `QOBUZ_USER_ID` — paid Qobuz account
  - `YANDEX_OAUTH_TOKEN` — [Yandex OAuth](https://oauth.yandex.ru/authorize?response_type=token&client_id=23cabbbdc6cd418abb4b39c32c41195d)
  - SquidWTF extras: `SQUIDWTF_SOURCE` (Qobuz/Tidal/AmazonMusic/Deemix),
    `SQUIDWTF_QUALITY` (default `6` = FLAC 16-bit)
- **Cache mode (default)**: fetched tracks are streamed and auto-cleaned after
  1h — **nothing is written to the library**. Set `STORAGE_MODE=Permanent` and
  uncomment the `/app/downloads` mount to keep downloaded albums (the organizer
  then tags/organizes them like any other album).
- **Client setup**: server type `Subsonic/OpenSubsonic`, URL
  `http://<your-nas>:4535/rest/` (or keep `:4534` for filtered-only, no missing
  tracks).
- **Dashboard**: set the plugin's `octoFiestaUrl` (e.g.
  `http://octo-fiesta:8080` on the shared network) and
  `octoFiestaProvider` (SquidWTF/Deezer/Qobuz/Yandex). The webhook reads them
  from the plugin status to probe octo-fiesta's health and show its Docker logs
  (via the read-only socket mount).
- **Optional admin creds** (`SUBSONIC_ADMIN_USERNAME/PASSWORD`) are only needed
  for Permanent-mode library registration; Cache mode works without them.
- Note: SquidWTF is a free third-party service and can be slow or intermittently
  down — octo-fiesta handles that with instance timeouts, but missing-track
  playback is best-effort.

### Acoustid (identity verification + ReplayGain)

Fingerprints audio files so songs without an MBID/ISRC can be accurately paired
to their album, and computes **ReplayGain** track gain/peak (ffmpeg EBU R128) for
the `writeReplayGain` tag setting. The image bundles `fpcalc` (chromaprint) and
`ffmpeg`.

> **Critical rule:** the sidecar's guest paths must **exactly match** the library
> paths Navidrome reports. The plugin sends `{library.path}/{relative}` (e.g.
> `/unsorted/Artist/Album/song.flac`). If a library is mounted at `/music` in
> Navidrome but the sidecar mounts it somewhere else, fingerprinting fails for
> that library. Mirror every `- host:guest` line.

Then in the plugin settings: `acoustidUrl = http://nd-organizer-acoustid:8097`
and `acoustidApiKey = <your AcoustID client key>` (free at
<https://acoustid.org/new-application>).

### Log dashboard (webhook)

A tiny web UI showing the plugin's status + reports (auto-refreshing). Open
`http://<your-nas>:8099/`. Plugin setting: `logWebhookUrl = http://<your-nas>:8099/`.

### Navidrome filter proxy

Drops filler-keyword tracks from every media response and limits skip-heavy content in
queued lists (exclude/third/lessThanHalf/half), re-sorts song lists by weight —
all **without touching files**. Point your Subsonic-compatible client at
`http://<your-nas>:4534/rest/` (or let octo-fiesta sit in front of it).

- **It's a faithful mirror**: every request is forwarded to Navidrome unchanged
  (same method, path, query, body, safe headers) — nothing is rewritten or
  dropped, including POST bodies (scrobble/star/setRating). The response is
  touched **only** when it's a JSON song list; XML responses, audio streams,
  cover art and errors pass back byte-for-byte, so non-JSON clients see an exact
  Navidrome. Only clients that request JSON (`f=json`) get filtering.
- Credentials pass through — use your normal Navidrome user/password in the client.
- Plugin flagging: set `filterUrl = http://nd-organizer-proxy:4534`; the plugin
  pushes the keyword list (`keywordFilterEnabled`), the skip-heavy ID set + limit
  mode (`skipContentMode`) and every track's weight via `POST /filters`.
- Optional env on the proxy: `FILTER_KEYWORDS` — startup default only. The
  plugin pushes Navidrome's `fillerKeywords` setting on every stats pass, so the
  Navidrome UI is the single source of truth for keyword filtering.
- Streaming (`stream`, `getCoverArt`, HLS) passes through byte-for-byte.

### Player setup (Subsonic clients)

All three entry points serve the **same Navidrome library** with your **normal
Navidrome user/password** (credentials pass through unchanged). In every player
set the server type to **`Subsonic/OpenSubsonic`** (Symfonium, DSub, Feishin,
Substreamer, Navidrome's own apps, ...).

| Port | What you get | Chain |
|---|---|---|
| **`4535`** — recommended | Full stack: queue filtering **and** missing-track fallback | player → octo-fiesta → filter proxy → navidrome |
| `4534` | Filtering only (filler-keyword drop + skip-content limits + weight re-sort), no missing-track fallback | player → filter proxy → navidrome |
| `4533` | Raw Navidrome, no proxy | player → navidrome |

URL format: `http://<your-nas>:<port>/rest/`.

- **`4535`** needs octo-fiesta deployed (and the plugin's `octoFiestaUrl` set so
  the webhook can monitor it). Songs Navidrome already has stream straight
  through; only tracks missing from the library are fetched on demand. This is
  the one to give your player.
- **`4534`** is a faithful mirror — responses are touched only when they are
  JSON song lists (keyword/skip filtering, reordering). Files are never moved.
- One URL per player — pick `4535` for the full experience; `4534` is the
  fallback if octo-fiesta is ever down (keep it as a second profile if you like).

### Building / publishing the images yourself

Images are published to GHCR automatically by the `.github/workflows/docker.yml`
workflow on every push to `main` (tag `:main`) and on `v*` git tags
(`:latest` + `X.Y.Z`). To build locally:

```bash
docker build -t nd-organizer-acoustid acoustid/
docker build -t nd-organizer-webhook webhook/
docker build -t nd-organizer-proxy proxy/
docker build -t nd-organizer-mysql mysql/
docker build -t nd-organizer-radio radio/
```

### Internet radio (optional)

Based on [WB2024/Add-Navidrome-Radios](https://github.com/WB2024/Add-Navidrome-Radios)
(CLI) as an always-on sidecar: search the **Radio-Browser** community database
and add stations directly into Navidrome's `radio` table — the same way the web
UI does — so they appear without a restart.

```bash
cd radio && docker compose up -d
```

- **It needs Navidrome's data dir** (the one with `navidrome.db`) mounted
  read-write at `/data`. Point `NAVIDROME_DB` at the `.db` inside it (default
  `/data/navidrome.db`).
- **Endpoints**: `GET /search?q=...&type=byname|bytag|bycountry`, `GET /top`,
  `GET /list`, `POST /add {"stations":[{name,url,homepage}]}` (dedups by
  name/url). Health at `GET /health`.
- **Dashboard**: the webhook shows a **Radio panel** (existing stations) and a
  `nd-organizer-radio` sidecar card once the sidecar is running.
- The sidecar is registered in `SIDECAR_LOG_PORTS` (port `8100`).

### Persistence: external MySQL / MariaDB

By default the plugin's cache/state (scan index, task log, stats, caches) lives in
the Navidrome-managed per-plugin SQLite file. To keep that state in **your own
database** instead, deploy the mysql sidecar and set the connection in the plugin
settings:

```bash
cd mysql && docker compose up -d
```

Then in **Plugins → nd-organizer → Settings → Persistence (MySQL)**:
`persistenceBackend = mysql`, `persistenceUrl = http://nd-organizer-mysql:8098`,
and your `mysqlHost/Port/Name/User/Password` (the `kvstore` table is created
automatically). The sidecar mirrors the host KVStore semantics exactly (256-byte
keys, blob values, TTLs), so switching backends is transparent — the same keys
are stored either way. Credentials are sent only to the sidecar over the internal
Docker network and live in Navidrome's config store, never in Docker env vars.

### Networking notes (containers reaching each other)

- Containers reach each other by **container name** only when they share a
  **user-defined network** (the `stack_network` network above). The host's LAN IP
  usually is **not** reachable from inside a container.
- If **Lidarr** or **AudioMuse-AI** also run as containers, attach them to the
  same `stack_network` network and set their URLs to container names:
  `lidarrUrl = http://lidarr:8686`, `audiomuseUrl = http://audiomuse-ai-flask-app:8000`.
- To attach an already-running container to the network without recreating it:
  ```bash
  docker network connect stack_network <container-name>
  ```
  Verify name resolution:
  ```bash
  docker exec navidrome getent hosts nd-organizer-acoustid
  docker exec navidrome getent hosts nd-organizer-webhook
  ```

## Install & configure

1. Copy `nd-organizer.ndp` into Navidrome's plugins folder
   (`<DataFolder>/plugins`, e.g. `./navidrome-data/plugins/`).
2. In the Navidrome UI: **Settings → Plugins → Rescan**, enable **Music
   Organizer**, and grant **Library Access** to the libraries you want organized.
3. **Grant write access** (no UI toggle) — from the Navidrome host:
   ```bash
   navidrome plugin edit nd-organizer --write-access --all-libraries
   ```
4. Configure in the UI:
   - **General**: leave `mode: dryRun` for now; set `runOnlyWhenIdle` (default on)
     and `scheduleCron` (e.g. `0 3 * * *`) or `runOnStartup`.
   - **Scanning**: set `scanUser` to a Navidrome admin username (used for
     `getNowPlaying` idle checks and the per-album `startScan`).
   - **Identity verification**: set `acoustidUrl` + `acoustidApiKey` (see Step 3).
   - **Metadata sources / Lidarr / AudioMuse**: enter your URLs and API keys.
     AudioMuse-AI is optional — a commented-out deploy block is included in the
     compose above (uncomment it, then set `audiomuseUrl =
     http://audiomuse-ai-flask-app:8000`); it needs ~8 GB RAM + a 4-core CPU.
5. Run a dry-run pass, review `report-*` / the webhook dashboard, then switch
   `mode: apply`.

On startup and after each run the log prints a **library inventory** (id, name,
path, `READ-WRITE`/`READ-ONLY`/`NO ACCESS`) so you can confirm which paths the
plugin can reach and write to.

## Rolling back a run

Every applied run records its changes under a **run ID**. To undo one:

1. Find the run ID — it's shown in the report/status (`runId`) and highlighted on
   the webhook dashboard ("Set `rollbackRunId` = …"), and every report ends with
   a `[rollback] Run ID:` line.
2. Set the plugin's **`rollbackRunId`** to that value.
3. Run a pass — the plugin reverses the moves (restores filenames/folders and
   the original `album.nfo` from backup), then clears the marker.

**What rollback restores:** file renames, folder moves, sidecar moves, and the
pre-write `album.nfo` content (captured before each rewrite). The full history
(`apply:` records + `backup:` snapshots) lives in the plugin KVStore.

**Retention:** rollback data is kept for **`rollbackRetentionDays`** (default
**30**; `0` = keep forever). Every pass prunes records older than that — so the
database stays lean without ever losing a run you can still roll back.
`backupRetentionDays` (default 30) similarly prunes reports/backups.

## Webhook dashboard

The `webhook` sidecar renders a self-refreshing dashboard at
`http://<host>:8099/` with collapsible sections:

- **Integrations** — the plugin's own health checks (AcoustID, Lidarr,
  AudioMuse-AI, MusicBrainz, Last.fm) with an alert banner when any need
  attention; the plugin re-checks at most once per 5 min (rate-limited).
- **Services** — independent liveness of every Docker sidecar (acoustid,
  proxy, mysql, webhook) via **heartbeats**. Each sidecar POSTs to the webhook
  every 60s when `WEBHOOK_URL` is set:
  ```yaml
  environment:
    - WEBHOOK_URL=http://nd-organizer-webhook:8099
  ```
  Green `UP` < 2 min, amber `WEAK` < 10 min, red `STALE` after that.
- **Status** — running/scanning/idle, per-library counts, **album plans**
  (kind badge, target folder, every `old → new` file move, dupes/fillers), and
  the **rollback callout** with the run ID.
- **Task queue** — recent task executions (scan chunks, plan batches, favsync,
  stats) with RUNNING/DONE/FAILED states.
- **Activity** — raw status/report posts; rows with failing integrations or
  warnings get an ISSUES/WARNINGS chip.

The plugin also posts a **stats heartbeat** every stats poll (5 min) so the
dashboard stays fresh between runs.

## Playback filtering (no file moves)

Nothing is ever moved into `_filler/` or `_skipped/` folders — albums stay
whole. Instead, a **Navidrome filter proxy** sits in front of Navidrome and
**deprioritizes and limits** flagged tracks in the song lists it returns. Point
your Subsonic-compatible client at the proxy instead of Navidrome;
credentials pass through unchanged.

```
client ──▶ filter proxy (:4534) ──▶ Navidrome (:4533)
                 │  keyword filter: drop filler titles from every media
                 │      response (album playlists, random, genre, starred,
                 │      similar, etc.) — explicit user searches keep them.
                 │  skip-content limiter (queues): keep at most
                 │     exclude / third / lessThanHalf / half skip-heavy tracks
                 │  re-sorts: search / random / starred / playlist song lists
                 │            by weight (plays − 2×skips), so skipped tracks
                 │            sink and liked tracks rise
```

The more a song is skipped, the less it is assumed to be liked: it sinks in
priority everywhere, and the **skip-content limiter** caps how much skip-heavy
content can populate a queued list (`skipContentMode`: `exclude` removes them,
`third`/`lessThanHalf`/`half` cap the fraction, `none` keeps everything). Albums
stay whole — `getAlbum` keeps full track order — and live/active views
(`getNowPlaying`, `getPlayQueue`) are never touched.

### Keyword filter (opt-in, `keywordFilterEnabled`)
Edit **Filler keywords** (`fillerKeywords`) in the Navidrome plugin settings.
The plugin pushes this list to the filter proxy on every stats pass, so the
proxy **drops keyword-matched tracks from every media response** (album track
lists, playlists, random, genre, starred, similar, top) — so intros and outros
never appear in any view. **Explicit user searches** (`searchResult*`) still
return keyword tracks. Files are never touched. (`FILTER_KEYWORDS`
on the proxy container is only a startup fallback.)

### Skip-content limiter (`skipContentMode`)
Every `statsPollMinutes` the plugin publishes each track's **weight** plus the
**skip-heavy ID set** to the proxy via `POST /filters` (apply mode). A track is
**skip-heavy** when it's a **net negative** — skipped strictly more times than
ever played in full (`plays < skips`, full plays forgive skips), 3+ interactions,
skip fraction at/above `skipHeavyRatio` (default 0.6). The proxy then limits how
many of these tracks can populate a queued list:

| Mode | Skip-heavy allowed in a queue |
|---|---|
| `exclude` | none (removed entirely) |
| `third` | up to a third of the list |
| `lessThanHalf` (default) | a bit less than half |
| `half` | up to half |
| `none` | everything (weight re-sort only) |

Songs you like but occasionally skip keep `plays ≥ skips`, so they're never
flagged — they just sink in priority and resurface when you play them again.

### Playback stats (opt-in)
Enable **Playback stats** (`playbackStatsEnabled`). Every `statsPollMinutes`
(minutes, default 5) the plugin:

1. **Watches `getNowPlaying`** between polls (no scrobbleretriever host needed —
   works on older Navidrome). A track that leaves playback having played less
   than **skipThresholdPercent** (default 30) of its duration is counted as a
   **skip**; leaving after the threshold is a **full play** (which also forgives
   one previous skip).
2. Computes a **weight** = plays − 2×skips and builds/updates the
   **"nd-organizer: Top Picks"** playlist (top `topPicksCount` songs by weight).
3. If a **Navidrome filter proxy URL** (`filterUrl`) is set, publishes every
   track's weight + the skip-heavy ID set (`skipContentMode`) + the filler
   keyword list (`keywordFilterEnabled`) to the proxy via `POST /filters`.

### Smart skip accounting

The skip signal self-corrects so it never permanently labels a song:

- **A full play forgives a skip.** Every observed full play (a track leaving
  playback after the skip threshold) decrements that track's skip count (never
  below 0). Skip it twice then play it in full — the next poll treats it as
  skipped once, not twice.
- **Skip-heavy only for net negatives.** A track is flagged skip-heavy only when
  it's skipped *strictly more times than it was ever played in full*
  (`plays < skips`) **and** its skip fraction reaches `skipHeavyRatio`
  (default 0.6, 3+ interactions). A song you like that you occasionally skip
  keeps `plays ≥ skips`, so it's never flagged — it just sinks in priority and
  resurfaces if you play it again.
- **Weight = plays − 2×skips** drives the Top Picks playlist ordering and the
  proxy's list re-sorting.

Use the Top Picks playlist as your "what to play next" source. All stats are
stored in the plugin KVStore.

> Note: skip detection is best-effort — it catches transitions observed between
> polls. A song started and finished entirely between polls isn't counted as a
> skip (its play is still counted).


## Build

Requirements: Rust stable + `wasm32-wasip1` target.

```powershell
rustup target add wasm32-wasip1
pwsh ./scripts/build.ps1          # -> dist/nd-organizer.ndp
pwsh ./scripts/build.ps1 -Install # build + copy to the Navidrome plugins share
```

Tests (host-side, no wasm needed):

```powershell
cargo test
```

The Navidrome Rust PDK (`nd-pdk` crates) is vendored under `pdk/` so the plugin
builds offline against a pinned API.

## Releasing a new version

Every release is **version-based**: bump the version in `manifest.json`, tag it,
and the `release.yml` workflow builds + attaches the `.ndp` to a GitHub Release
for download (it verifies `manifest.version` matches the tag, so a mismatch
fails the build instead of publishing an inconsistent artifact).

```bash
# 1. bump "version" in manifest.json (e.g. 0.1.0 -> 0.2.0)
git add manifest.json && git commit -m "release 0.2.0"
git tag v0.2.0 && git push origin v0.2.0
# 2. grab dist/nd-organizer-0.2.0.ndp from the release page
```

Pushing the tag also re-publishes the sidecar Docker images
(`ghcr.io/lunatixz/nd-organizer/*:latest` + `X.Y.Z`) via `docker.yml`.

## Security notes

- Library mounts are **read-only unless you grant write access** via
  `navidrome plugin edit nd-organizer --write-access`. Only grant it to plugins
  you trust.
- `http.requiredHosts` is `["*"]` because Lidarr/AudioMuse/AcoustID URLs are
  user-configured per install (same precedent as `nd-lyrics`). The plugin only
  makes outbound requests to hosts you put in its config.
- Plugin config (API keys) is stored in Navidrome's config store.

## Known limitations

- AudioMuse-AI acoustic features only exist for tracks it has already analyzed.
- AcoustID requires the **Docker sidecar** to be reachable; without it, files
  lacking an MBID/ISRC are routed to Singles (unverified) rather than guessed.
- First full scan of a large library takes a while by design (accuracy over
  speed); subsequent passes are incremental.
- After renames, Navidrome favorites/playcounts survive only if **Persistent
  IDs** are enabled in Navidrome config.

## Sponsor

If this plugin saves you time, consider buying me a coffee. Development and
maintenance happens in my spare time — your support keeps it going.

[![Sponsor](https://img.shields.io/badge/Sponsor-GitHub%20Sponsor-ea4aaa.svg)](https://github.com/sponsors/lunatixz)

## Built with the help of AI

This project was developed with the assistance of an AI coding agent. The
architecture, code, tests, and documentation were all produced through a
collaborative process between human intent and machine execution. The result is
a codebase that's been reviewed, tested, and shipped — the AI didn't just
generate code, it helped build a product.

