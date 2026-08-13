# nd-organizer

A [Navidrome](https://www.navidrome.org/) plugin (Rust → WebAssembly, packaged as
`.ndp`) that organizes your music library — slowly and accurately:

- **Full-library scan** reads every file's tags into a persistent index, then
  groups files into albums by their **metadata** (MusicBrainz IDs / ISRC), not
  by their folders — so scattered songs that belong to one album are recognized
  as one album instead of a pile of "singles".
- **Identity verification before anything moves**: files with no reliable ID
  (MBID/ISRC) are fingerprinted via an **AcoustID sidecar** (Docker) so a song
  is only paired to an album when it's actually identified. Anything still
  unverifiable is left in place and reported.
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
  detected and flagged so the **Subsonic filter proxy** drops them from client
  playback. **Albums stay whole and files are never moved.**
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
   Files that still can't be identified are left in place (reported).
3. **Group** — verified files are grouped into albums by album MBID, or by
   album artist + album + year.
4. **Plan** — each album group is planned in batches (`albumsPerTask`): target
   folder/file names from your schemas, duplicate detection.
5. **Apply** (only in `mode: apply`) — moves files + sidecars, handles
   duplicates, records every change for rollback, and triggers a Navidrome
   rescan per album.

**Safety model:** `mode: dryRun` is the default — it plans and reports but writes
nothing. Review the report/status, then switch to `apply`.

## Docker setup (docker-compose)

The plugin itself is a `.ndp` file in Navidrome's plugins folder. Three optional
**sidecar images** run as containers:

| Image | Purpose |
|---|---|
| `ghcr.io/lunatixz/nd-organizer/acoustid:latest` | Fingerprints songs (AcoustID) so unverified files can be paired to their album. |
| `ghcr.io/lunatixz/nd-organizer/webhook:latest` | A web dashboard showing status + reports (auto-refreshing). |
| `ghcr.io/lunatixz/nd-organizer/proxy:latest` | Subsonic filtering proxy — sits in front of Navidrome; drops filler tracks from the queue and skip-heavy tracks past the cap, re-sorts lists by weight, without touching files. |
| `ghcr.io/lunatixz/nd-organizer/mysql:latest` | Optional MySQL bridge — executes the plugin's kvstore operations against your MySQL/MariaDB when `persistenceBackend = mysql`. |

The compose files below reference the published GHCR images — `docker compose up`
pulls them (no local build, no build context needed). Tags: `:latest`, `:main`,
and `vX.Y.Z` semver tags per release.

### Step 1 — a shared Docker network

Create one user-defined network so Navidrome can reach the sidecars (and your
other containers) **by container name**:

```bash
docker network create stack_network
```

### Step 2 — Navidrome (mount every library read-write, join the network)

With multiple libraries, mount **each library path** as its own volume and note
the guest path (that becomes the library's path in Navidrome, e.g. `/music`,
`/unsorted`). `docker-compose.yml`:

```yaml
services:
  navidrome:
    image: deluan/navidrome:latest
    container_name: navidrome
    ports:
      - "4533:4533"
    environment:
      - ND_PLUGINS_ENABLED=true
      - ND_PLUGINS_AUTORELOAD=true
      # Optional: agents the plugin can power for the UI (see README/agents):
      # - ND_AGENTS=nd-organizer
    volumes:
      - ./navidrome-data:/data
      - /path/to/music:/music:rw        # library 1  (path = /music)
      - /path/to/unsorted:/unsorted:rw  # library 2  (path = /unsorted)
      # add more - /host/path:/guest:rw lines for every library
    networks:
      - stack_network

networks:
  stack_network:
    external: true
    name: stack_network
```

- **`rw`** is required (the plugin renames/tags files).
- Copy `nd-organizer.ndp` into `./navidrome-data/plugins/`.
- Create the libraries in the Navidrome UI (**Settings → Libraries**) with those
  paths.

### Step 3 — Acoustid sidecar (recommended, powers identity verification)

Fingerprints audio files so songs without an MBID/ISRC can be accurately paired
to their album. **It must mount EVERY library at the SAME guest paths Navidrome
uses**, so the file paths the plugin sends match. `acoustid/docker-compose.yml`:

```yaml
services:
  nd-organizer-acoustid:
    image: ghcr.io/lunatixz/nd-organizer/acoustid:latest
    container_name: nd-organizer-acoustid
    restart: unless-stopped
    ports:
      - "8097:8097"
    volumes:
      - /path/to/music:/music:ro        # mirror library 1, same path as Navidrome
      - /path/to/unsorted:/unsorted:ro  # mirror library 2, same path as Navidrome
      # mirror every library from Step 2 here, with the SAME guest paths
    networks:
      - stack_network

networks:
  stack_network:
    external: true
    name: stack_network
```

```bash
cd acoustid && docker compose up -d        # pulls the GHCR image
```

> **Critical rule:** the sidecar's guest paths must **exactly match** the library
> paths Navidrome reports. The plugin sends `{library.path}/{relative}` (e.g.
> `/unsorted/Artist/Album/song.flac`). If a library is mounted at `/music` in
> Navidrome but the sidecar mounts it somewhere else, fingerprinting fails for
> that library. Mirror every `- host:guest` line.

Then in the plugin settings: `acoustidUrl = http://nd-organizer-acoustid:8097`
and `acoustidApiKey = <your AcoustID client key>` (get one free at
<https://acoustid.org/new-application>).

### Step 4 — Log dashboard (optional)

A tiny web UI that shows the plugin's status + reports (auto-refreshing).
`webhook/docker-compose.yml`:

```yaml
services:
  nd-organizer-webhook:
    image: ghcr.io/lunatixz/nd-organizer/webhook:latest
    container_name: nd-organizer-webhook
    restart: unless-stopped
    ports:
      - "8099:8099"
    networks:
      - stack_network

networks:
  stack_network:
    external: true
    name: stack_network
```

```bash
cd webhook && docker compose up -d
```

Then in the plugin settings: `logWebhookUrl = http://<your-nas>:8099/`.
Open `http://<your-nas>:8099/` in a browser to watch activity.

### Step 5 - Subsonic filter proxy (optional, playback filtering)

Drops filler tracks from the queue and skip-heavy tracks past the cap, and
re-sorts song lists by weight — all **without touching files**. Deploy it and
point your Subsonic-compatible client at it instead of Navidrome:

```bash
cd proxy && docker compose up -d
```

- **It's a faithful mirror**: every request is forwarded to Navidrome unchanged
  (same method, path, query, body, safe headers) — nothing is rewritten or
  dropped, including POST bodies (scrobble/star/setRating). The response is
  touched **only** when it's a JSON song list; XML responses, audio streams,
  cover art and errors pass back byte-for-byte, so non-JSON clients see an exact
  Navidrome. Only clients that request JSON (`f=json`) get filtering.
- Credentials pass through — use your normal Navidrome user/password in the client.
- Client setup: server type `Subsonic/OpenSubsonic`, URL `http://<your-nas>:4534/rest/`.
- Plugin flagging: set `filterUrl = http://nd-organizer-proxy:4534` and enable
  `skipIgnoreEnabled` so weights + skip-heavy IDs are published via `POST /filters`
  (re-sorts lists, hard-removes only net-negative tracks).
- Optional env on the proxy: `FILTER_KEYWORDS` — startup default only. The
  plugin pushes Navidrome's `fillerKeywords` setting on every stats pass, so the
  Navidrome UI is the single source of truth for keyword filtering.
- Streaming (`stream`, `getCoverArt`, HLS) passes through byte-for-byte.

### Building / publishing the images yourself

Images are published to GHCR automatically by the `.github/workflows/docker.yml`
workflow on every push to `main` (tag `:main`) and on `v*` git tags
(`:latest` + `X.Y.Z`). To build locally:

```bash
docker build -t nd-organizer-acoustid acoustid/
docker build -t nd-organizer-webhook webhook/
docker build -t nd-organizer-proxy proxy/
docker build -t nd-organizer-mysql mysql/
```

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
  **user-defined network** (the `stack_network` network above). The host's LAN IP (e.g.
  `192.168.0.21`) usually is **not** reachable from inside a container.
- If **Lidarr** or **AudioMuse-AI** also run as containers, attach them to the
  same `stack_network` network and set their URLs to container names:
  `lidarrUrl = http://lidarr:8686`, `audiomuseUrl = http://audiomuse:8000`.
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
whole. Instead, a **Subsonic filter proxy** sits in front of Navidrome and both
**deprioritizes and drops** flagged tracks in the song lists it returns. Point
your Subsonic-compatible client at the proxy instead of Navidrome;
credentials pass through unchanged.

```
client ──▶ filter proxy (:4534) ──▶ Navidrome (:4533)
                 │  drops (in the queue): filler titles (keyword match)
                 │        (everywhere):   skip-heavy tracks past the cap
                 │                        (net-negative: skipped more than
                 │                         ever played in full)
                 │  re-sorts: search / random / starred / playlist
                 │            song lists by weight (plays − 2×skips),
                 │            so skipped tracks sink and liked tracks rise
```

The more a song is skipped, the less it is assumed to be liked: it sinks in
priority everywhere, and only a genuinely disliked track (skipped strictly more
times than it was ever played in full, past your cap) is removed. Albums stay
whole — `getAlbum` keeps full track order (only net-negative tracks drop out),
and live/active views (`getNowPlaying`, `getPlayQueue`) are never touched.

### Filler tracks (opt-in)
Edit **Filler keywords** (`fillerKeywords`) in the Navidrome plugin settings.
The plugin pushes this list to the filter proxy on every stats pass, so the
proxy **ignores keyword-matched tracks from the queue**: dropped from auto-queue
lists (random, search, playlists, genres, top/similar) so intros and outros
never auto-play — but an album's track list stays whole, so you can still play
them deliberately from their album. Files are never touched. (`FILTER_KEYWORDS`
on the proxy container is only a startup fallback.)

### Playback stats + skip flagging (opt-in)
Enable **Playback stats** (`playbackStatsEnabled`). Every `statsPollMinutes`
(minutes, default 5) the plugin:

1. **Watches `getNowPlaying`** between polls (no scrobbleretriever host needed —
   works on older Navidrome). A track that leaves playback having played less
   than **skipThresholdPercent** (default 30) of its duration is counted as a
   **skip**; leaving after the threshold is a **full play** (which also forgives
   one previous skip).
2. Computes a **weight** = plays − 2×skips and builds/updates the
   **"nd-organizer: Top Picks"** playlist (top `topPicksCount` songs by weight).
3. If **Exclude skip-heavy tracks** (`skipIgnoreEnabled`) + **filter proxy URL**
   (`filterUrl`) are set, publishes every track's weight to the proxy via
   `POST /filters` — it re-sorts returned lists by weight (skipped tracks sink,
   liked tracks rise).

### Smart skip accounting

The skip signal self-corrects so it never permanently labels a song:

- **A full play forgives a skip.** Every observed full play (a track leaving
  playback after the skip threshold) decrements that track's skip count (never
  below 0). Skip it twice then play it in full — the next poll treats it as
  skipped once, not twice.
- **Hard removal only for net negatives.** The proxy hard-removes a track from
  client playback only when it's skipped *strictly more times than it was ever
  played in full* (`plays < skips`) **and** its skip fraction reaches
  `skipIgnoreRatio` (default 0.6, 3+ interactions). A song you like that you
  occasionally skip keeps `plays ≥ skips`, so it's never removed — it just sinks
  in priority and resurfaces if you play it again.
- **Weight = plays − 2×skips** drives the Top Picks playlist ordering and the
  proxy's list re-sorting.

Use the Top Picks playlist as your "what to play next" source. All stats are
stored in the plugin KVStore.

> Note: skip detection is best-effort — it catches transitions observed between
> polls. A song started and finished entirely between polls isn't counted as a
> skip (its play is still counted).

## Config reference

| Key | Default | Meaning |
|---|---|---|
| `mode` | `dryRun` | `dryRun` previews; `apply` writes. |
| Libraries | (permission) | Which libraries to organize = the Navidrome **Library Access** permission. |
| `runOnStartup` / `scheduleCron` | `false` / `""` | How runs are triggered. |
| `albumsPerTask` | `5` | Album groups planned per background task (small = lean). |
| `filesPerScanTask` | `200` | Files scanned (tags read) per task. Lower = lighter/slower. |
| `runOnlyWhenIdle` | `true` | Defer runs while something is playing. |
| `folderSchema` | `{albumArtist}/{album} ({year})` | Album folder template. |
| `fileSchema` | `{track:02} - {title}` | Track file template (ext auto-added). |
| `soundtrackFolder`/`variousFolder`/`singlesFolder` | `Sound Tracks`/`Various Artist`/`Singles` | Bucket folder names. |
| `singlesUnderArtist` | `true` | `Artist/Singles/{title}` vs `Various Artist/Singles/...`. |
| `preserveRecordingType` | `true` | Append `(Live)`/`(Bootleg)` so live tracks stay distinct. |
| `excludePaths` | `[]` | Paths never to touch (e.g. `inbox`, `Downloads/*`). |
| `readNfo` / `writeNfo` | `true` / `false` | NFO sidecar read / rewrite. |
| `backupBeforeWrite` / `backupRetentionDays` | `true` / `30` | Metadata-only pre-write snapshots + retention. |
| `rollbackRetentionDays` | `30` | How long apply records + file/nfo backups are kept for rollback (`0` = forever); old runs pruned automatically. |
| `verifyIdentity` / `skipUnverified` | `true` / `true` | Require MBID/ISRC/AcoustID identity before pairing; leave others in place. |
| `acoustidUrl` / `acoustidApiKey` | — | AcoustID sidecar URL + client key. |
| `lidarrUrl`/`lidarrApiKey`/`lidarrMode` | — | Lidarr metadata/classification, naming schema, monitored force-search. |
| `useLidarrNamingSchema` | `false` | Use Lidarr's naming config instead of `folderSchema`/`fileSchema`. |
| `lidarrForceSearchIncomplete` | `false` | AlbumSearch for incomplete albums whose artist+album are monitored. |
| Lidarr post-move refresh | — | With `lidarrMode = metadataPlusRescan`, after moving an album the plugin fires Lidarr `RefreshArtist` for that artist (once per artist / 5 min) so Lidarr's DB follows the new paths. |
| `audiomuseUrl`/`audiomuseToken` | — | AudioMuse-AI acoustic tags + re-sync. |
| `scanUser` / `triggerScanAfterRun` / `scanAfterAlbum` | — | Navidrome admin user + per-album rescans. |
| `rollbackRunId` | `""` | Set to a run ID to undo that run. |
| `fillerKeywords` | `intro,outro,interlude,...,skit,instrumental,interview` | Titles matching one (whole-word) are ignored from the queue by the filter proxy. Files never move. |
| `playbackStatsEnabled` / `statsPollMinutes` | `false` / `5` | Track plays/skips; update cadence. |
| `skipThresholdPercent` | `30` | Played less than this % = a skip. |
| `topPicksCount` | `50` | Size of the `nd-organizer: Top Picks` playlist. |
| `skipIgnoreEnabled` / `skipIgnoreRatio` | `false` / `0.6` | Publish weights + IDs to the proxy: reorder lists by weight; hard-remove only net-negative tracks (skips > full plays, past ratio, 3+ samples). Full plays forgive skips. |
| `filterUrl` | `""` | Subsonic filter proxy base URL (e.g. `http://nd-organizer-proxy:4534`); receives `POST /filters`. |

Template placeholders: `{track}` `{disc}` `{title}` `{artist}` `{albumArtist}`
`{album}` `{year}` `{genre}` `{recording}` `{mbid}`, plus `:NN` zero-padding
(`{track:02}`). Filenames are sanitized for all OSes.

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
  lacking an MBID/ISRC are left in place (unverified) rather than guessed.
- First full scan of a large library takes a while by design (accuracy over
  speed); subsequent passes are incremental.
- After renames, Navidrome favorites/playcounts survive only if **Persistent
  IDs** are enabled in Navidrome config.

