# AGENTS.md — nd-organizer

Guide for AI coding agents working in this repo. Read this first.

> **STOP — READ THIS BEFORE DOING ANYTHING ELSE**
>
> 1. **NEVER overwrite the full plugin config in navidrome.db.** The `plugin.config`
>    column contains the user's API keys, passwords, and tokens. Writing to it
>    destroys secrets permanently. Use `json_set()` for targeted patches only.
>
> 2. **NEVER restart Navidrome after deploying a new .ndp file.** Navidrome
>    resets plugin config to defaults whenever it detects the .ndp changed.
>    This has wiped the user's settings TWICE (2026-09-06). Let the user
>    re-enable the plugin through the UI instead.
>
> 3. **Backup config before ANY DB operation.** Run this first:
>    ```sql
>    sqlite3 /data/navidrome.db "SELECT config FROM plugin WHERE id='nd-organizer';"
>    ```
>    Save the output. If anything goes wrong, restore with:
>    ```sql
>    UPDATE plugin SET config = '<saved_json>' WHERE id='nd-organizer';
>    ```

## What this is

A **Navidrome plugin** (Rust → WASM, packaged as a `.ndp`) that organizes a music
library: scans files, verifies identities (MusicBrainz/ISRC/AcoustID), groups
into albums, plans+applies folder/file renames and tag writes (with rollback),
and tracks playback stats + 0–5 star ratings with loved-status and album ratings.
It ships with **sidecar Docker services** (Python) for capabilities the WASM
sandbox can't do: audio fingerprinting + ReplayGain (acoustid), a web dashboard
(webhook), a Subsonic filter proxy, MySQL KV persistence, and a
missing-track proxy.

## Architecture at a glance

```
nd-organizer.ndp  (Rust -> wasm32-wasip1, packaged manifest.json + plugin.wasm)
  src/
    lib.rs         wasm glue: lifecycle, scheduler, run_pass, integration health
    config.rs      Config struct (parsed from Navidrome's flat key->string map)
    scan.rs        scan -> verify (AcoustID) -> group -> plan -> apply + cleanup
    organizer.rs   grouping/duplicate detection/nfo apply (pure, host-tested)
    tags.rs        lofty tag read/write (atomic temp+rename), MBID/playback/replaygain
    stats.rs       playback stats, star tallies (0-5), loved status, album ratings, Top Picks, filters publish
    favorites.rs   Last.fm loved/playcount/scrobble + ListenBrainz scrobble, two-way sync
    lidarr.rs      Lidarr API: album lookup, ratings, incomplete search, rescan
    musicbrainz.rs MusicBrainz release lookup + release tracklist (auto-tag) + genre fetch
    artwork.rs     Cover Art Archive, Apple Music, TheAudioDB artwork fetch + fallback chain
    apple_music.rs Apple Music / iTunes: artwork, artist images, biographies
    theaudiodb.rs  TheAudioDB: fanart, bios, album artwork, genre tags
    discogs.rs     Discogs: credits, community ratings, genre/style tags
    lyrics.rs      LRCLIB lyrics
    audiomuse.rs   AudioMuse-AI acoustic tags (BPM/key/mood), URL resolve (host fallback)
    net.rs         generic circuit breaker + throttled cached HTTP + circuit_check
    nfo.rs         Kodi-style album/artist NFO read/write
    template.rs    {placeholder:format} path templates
    state.rs       KV keys, backups, fnv1a64 hash, rollback
    store.rs       Host (Navidrome KVStore) / Mysql backends + migration
    identity.rs    verification confidence scoring
    report.rs      plain-language run reports
    trim.rs        purge Navidrome's missing-files list (DELETE /api/missing)

sidecars/  (Python, each its own dir + Dockerfile + docker-compose.yml)
  acoustid/   fpcalc (chromaprint) + ffmpeg ReplayGain; POST /lookup, /replaygain
  webhook/    dashboard + sidecar logs + radio management; /radio-search, /radio-add
  proxy/      Subsonic filter (keyword/skip-content), /filters, /status
  mysql/      KVStore -> MySQL bridge (executes kv ops)
```

## Key commands

```bash
# Host unit tests
cargo test

# Full check: tests + clippy (all targets) + wasm build
pwsh ./scripts/test.ps1

# Clippy with -D warnings (used before shipping)
cargo clippy --all-targets -- -D warnings

# Wasm build check
cargo check --target wasm32-wasip1

# Build + package the .ndp (manifest.json + plugin.wasm in a zip)
pwsh ./scripts/build.ps1

# Build + install .ndp to the NAS plugin share + install .ndp
pwsh ./scripts/build.ps1 -Install
```

**Always run before finishing a change**: `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo check --target wasm32-wasip1`. Keep the test count passing (currently ~70).

## Release workflow

Two tracks: **stable releases** and **nightly builds**.

**Every commit to main (nightly):**
1. `latest` tag must point to HEAD — `git tag -f latest && git push origin -f latest`
2. Changelog must reflect current state of main
3. `.ndp` is rebuilt and installed to NAS
4. GitHub Actions uploads only `nd-organizer.ndp` to the "latest" release (clobber)
5. The versioned release (vX.Y.Z) is NEVER touched by nightly commits

**Version bump only (stable release):**
1. Bump version in `Cargo.toml` and `manifest.json`
2. Build + install `.ndp`
3. Commit + push
4. Create version tag — `git tag -a vX.Y.Z -m "vX.Y.Z: <summary>"`
5. Push tag — `git push origin vX.Y.Z`
6. Also update `latest` tag to HEAD
7. GitHub Actions creates a NEW release for that version with `nd-organizer-X.Y.Z.ndp` (frozen, never updated)
8. Sidecar Docker images are rebuilt for the tag

**Rule: stable releases are frozen.** Once vX.Y.Z is released, that release and its .ndp are never updated. Only `latest` receives ongoing updates.

## Critical constraints & gotchas

- **WASM sandbox**: the plugin runs in Navidrome's WASM runtime. It has **no
  filesystem access to Navidrome's data dir** (can't read/write `navidrome.db`).
  Host-only APIs come from `nd_pdk::host` (http, kvstore, subsonicapi, config,
  library, users, scheduler, task, cache, etc.). Anything needing raw SQLite or
  audio decode lives in a **sidecar**.
- **wasm-gated modules**: `scan.rs`, `net.rs`, `artwork.rs`, `audiomuse.rs`,
  `lyrics.rs`, `musicbrainz.rs`, `store.rs`, `trim.rs` are
  `#[cfg(target_arch = "wasm32")]` — they are NOT compiled on the host, so
  `cargo test` doesn't exercise them. Host-tested logic lives in `config.rs`,
  `organizer.rs`, `tags.rs`, `stats.rs`, `favorites.rs`, `lidarr.rs`, `nfo.rs`,
  `template.rs`, `identity.rs`, `report.rs`, `state.rs`. Beware: a wasm-only
  change can compile on host tests yet break the wasm build — always run the
  wasm check.
- **Config model**: Navidrome stores plugin config as a flat `map<String,String>`
  (camelCase keys). `Config::default()` is the base; `from_map` overrides.
  Every manifest property must be (a) in `manifest.json`
  `config.schema.properties`, (b) placed in at least one `uiSchema` group
  (settings can be in MULTIPLE groups if they relate to multiple functions),
  and (c) parsed in `config.rs` `from_map` and used via `cfg.<field>` in logic.
  Settings that relate to several functions belong in **every** relevant group —
  do not remove them from one group when adding to another.
- **Manifest/UI**: `manifest.json` drives the Navidrome settings UI. Keep the
  uiSchema groups semantic (a setting in the groups it relates to). Validate
  after edits: `node -e "JSON.parse(require('fs').readFileSync('manifest.json','utf8'))"`.
- **Circuit breaker** (`src/net.rs`): external HTTP providers (musicbrainz,
  lidarr, lastfm, listenbrainz, lrclib, coverartarchive, audiomuse, acoustid) are
  protected by a generic circuit (retry 5m → cooldown 30m → degraded). **Trip
  ONLY on transport failure / no response / 5xx. 404 / 4xx / no-data must NOT
  trip** — those mean the service is up but had nothing for us.
- **Tag writes are atomic** (`tags::atomic_write` / `save_tagged_atomic`):
  temp file + fsync + rename. Never write in place.
- **Star rating (0–5.0, half steps)**: full listen (≥ `starFullPlayPercent`)
  = +1.0 and +1 playcount and forgives one skip; half (≥ `starHalfPlayPercent`)
  = +0.5; below `starIgnorePercent` = ignored (no penalty); else skip = −0.5.
  Capped 0–5. Loved = rating ≥ `lovedThresholdStars`. Initial rating seeds from
  Navidrome playCount + Last.fm playcount/loved + Lidarr track/album rating.
  Album ratings average track stars. Loved status published to Navidrome
  (star/unstar) and file tags.
- **Rating sync**: ratings propagate bidirectionally between the plugin DB,
  Navidrome, Last.fm, and Lidarr. The plugin DB is the canonical source.
  `ratingSyncWriteToLidarr` pushes track + album ratings to Lidarr on every
  stats pass. `ratingSyncPullFromNavidrome` imports Navidrome's setRating
  values into the plugin DB (useful when ratings are set in the UI).
  Favorites sync (`favoritesSyncLastfm`) handles Navidrome ↔ Last.fm loved
  bidirectionally. New rating sources should follow the pattern: seed on
  first observation in `record_star_listen`, publish outward in
  `publish_star_ratings`, add config toggle + manifest prop.
- **Scrobble**: fires to **both** Last.fm (`track.scrobble`) and ListenBrainz
  (`submit-listens`) on full plays, gated by `lastfmScrobble` and
  `listenbrainzScrobble` respectively. Never scrobbles silently — both flags
  default off to avoid double-counting.
- **The webhook must never iterate all accumulated events**: `entries` is
  capped at `MAX_ENTRIES` (2000), `load_log` reads only the tail and self-cleans,
  render loops are bounded (`reversed(entries[:N])` / `entries[-5000:]`), and
  sidecar fetches respect a per-render deadline (`_render_deadline`). A 500k-line
  backlog must still render in seconds — do not reintroduce unbounded full-list
  scans.
- **Python sidecars**: `webhook/`, `proxy/`, `mysql/`, `acoustid/`
  each have `server.py` + `Dockerfile` + `docker-compose.yml`. They must be
  Python 3.12-compatible (the base image), and response writes swallow
  `BrokenPipeError`/`ConnectionResetError` (a `_wfile_write` helper). Validate
  with `python -c "import ast; ast.parse(open('.../server.py').read())"`.

## Deployment model

- **Plugin `.ndp`**: built locally with `build.ps1`, copied to the Navidrome
  plugins share (`\\192.168.0.21\opt\navidrome\data\plugins\`), rescan plugins.
  Also built/published by GitHub Actions `release.yml` to a GitHub Release.
- **Sidecar images**: built + pushed to GHCR by `.github/workflows/docker.yml`
  (matrix: acoustid, webhook, proxy, mysql). The NAS deploys via
  `docker-compose.yml` pulling `ghcr.io/.../sidecar:latest`. **If you change a
  sidecar's `server.py`, the container on the NAS must be redeployed** (pull +
  recreate) — a stale image is the usual cause of "the dashboard is old."
- Sidecars reach each other by container name on `stack_network` (external
  docker network that also includes Navidrome). Static container IPs break on
  recreate — prefer container names. AudioMuse-AI containers use the upstream
  names (`audiomuse-ai-flask-app`, `audiomuse-ai-worker-instance`) not
  `nd-organizer-*`.
- Commit + push to `main` triggers docker.yml (re)builds. Keep the working tree
  clean and the committed state shippable.

## Documentation

- `README.md` is the user-facing doc (pipeline, star system, docker setup,
  player setup, sidecars, install/config). Update it when behavior/settings
  change. The "Config reference" section was removed — settings are documented
  via manifest descriptions.
- `docker-compose.yml` is the single-source-of-truth compose (Navidrome + all
  six sidecars); the README's compose block must stay byte-identical to it.
- `.env.example` documents the compose `${VAR}` values (octo-fiesta).

## Secrets

API keys/tokens/passwords live in **Navidrome's plugin config**, never in
Docker env or committed files (the `.env.example` ships empty placeholders).
Don't log tokens/passwords.

## NEVER overwrite plugin config in the DB

**2026-09-06 incident**: An AI agent wrote the full `Config::default()` JSON
back to `navidrome.db`'s `plugin.config` column via `UPDATE plugin SET config = ...`
to fix a `mode` field. This destroyed all of the user's API keys, passwords,
and custom settings (AcoustID, Last.fm, Lidarr, AudioMuse, Discogs, Genius,
MusicBrainz, TheAudioDB tokens). There was no backup.

**Rule**: NEVER write the full config JSON to the DB. The `config` column
contains the user's secrets. If you need to change a single field, use targeted
SQL like:
```sql
UPDATE plugin SET config = json_set(config, '$.mode', 'Apply') WHERE id = 'nd-organizer';
```
Or better: fix the issue in code (config parser, manifest defaults) and let
the user re-enable the plugin. The config column is a black box — treat it as
read-only from SQL.

**CRITICAL: Navidrome resets plugin config on .ndp file change.** When the
`.ndp` file changes (new deployment), Navidrome detects it and resets the
plugin config to defaults. This has happened twice (2026-09-06) and wiped all
API keys/passwords both times. **NEVER restart Navidrome after deploying a
new .ndp** unless you first backup the config:
```sql
sqlite3 /data/navidrome.db ".read /dev/stdin" << 'EOF'
SELECT config FROM plugin WHERE id = 'nd-organizer';
EOF
```
Or use `json_set()` for targeted patches. If the config IS wiped, restore
via:
```sql
UPDATE plugin SET config = '<saved_json>' WHERE id = 'nd-organizer';
```