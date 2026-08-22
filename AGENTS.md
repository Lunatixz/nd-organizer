# AGENTS.md — nd-organizer

Guide for AI coding agents working in this repo. Read this first.

## What this is

A **Navidrome plugin** (Rust → WASM, packaged as a `.ndp`) that organizes a music
library: scans files, verifies identities (MusicBrainz/ISRC/AcoustID), groups
into albums, plans+applies folder/file renames and tag writes (with rollback),
and tracks playback stats + 0–5 star ratings. It ships with **sidecar Docker
services** (Python) for capabilities the WASM sandbox can't do: audio
fingerprinting + ReplayGain (acoustid), a web dashboard (webhook), a Subsonic
filter proxy, MySQL KV persistence, internet radio, and a missing-track proxy.

## Architecture at a glance

```
nd-organizer.ndp  (Rust -> wasm32-wasip1, packaged manifest.json + plugin.wasm)
  src/
    lib.rs         wasm glue: lifecycle, scheduler, run_pass, integration health
    config.rs      Config struct (parsed from Navidrome's flat key->string map)
    scan.rs        scan -> verify (AcoustID) -> group -> plan -> apply + cleanup
    organizer.rs   grouping/duplicate detection/nfo apply (pure, host-tested)
    tags.rs        lofty tag read/write (atomic temp+rename), MBID/playback meta
    stats.rs       playback stats, star tallies (0-5), Top Picks, filters publish
    favorites.rs   Last.fm loved/playcount/scrobble sync
    lidarr.rs      Lidarr API: album lookup, ratings, incomplete search, rescan
    musicbrainz.rs MusicBrainz release lookup + release tracklist (auto-tag)
    artwork.rs     Cover Art Archive fetch/embed/cover.jpg
    lyrics.rs      LRCLIB lyrics
    audiomuse.rs   AudioMuse-AI acoustic tags (BPM/key/mood), URL resolve
    net.rs         generic circuit breaker + throttled cached HTTP
    nfo.rs         Kodi-style album/artist NFO read/write
    template.rs    {placeholder:format} path templates
    state.rs       KV keys, backups, fnv1a64 hash, rollback
    store.rs       Host (Navidrome KVStore) / Mysql backends + migration
    identity.rs    verification confidence scoring
    report.rs      plain-language run reports
    trim.rs        purge Navidrome's missing-files list (DELETE /api/missing)

sidecars/  (Python, each its own dir + Dockerfile + docker-compose.yml)
  acoustid/   fpcalc (chromaprint) + ffmpeg ReplayGain; POST /lookup, /replaygain
  webhook/    dashboard; POST /status receiver; /logs, /radio-search, /radio-add
  proxy/      Subsonic filter proxy (keyword/skip-content), /filters, /status
  mysql/      KVStore -> MySQL bridge (executes kv ops)
  radio/      internet radio (Radio-Browser -> Navidrome `radio` table)
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
  lidarr, lastfm, lrclib, coverartarchive, audiomuse, acoustid) are protected by
  a generic circuit (retry 5m → cooldown 30m → degraded). **Trip ONLY on
  transport failure / no response / 5xx. 404 / 4xx / no-data must NOT trip** —
  those mean the service is up but had nothing for us.
- **Tag writes are atomic** (`tags::atomic_write` / `save_tagged_atomic`):
  temp file + fsync + rename. Never write in place.
- **Star rating (0–5.0, half steps)**: full listen (≥ `starFullPlayPercent`)
  = +1.0 and +1 playcount and forgives one skip; half (≥ `starHalfPlayPercent`)
  = +0.5; below `starIgnorePercent` = ignored (no penalty); else skip = −0.5.
  Capped 0–5. Loved = rating ≥ `lovedThresholdStars`. Initial rating seeds from
  Navidrome playCount + Last.fm playcount/loved + Lidarr track/album rating.
- **The webhook must never iterate all accumulated events**: `entries` is
  capped at `MAX_ENTRIES` (2000), `load_log` reads only the tail, render loops
  are bounded (`reversed(entries[:N])` / `entries[-5000:]`), and sidecar fetches
  respect a per-render deadline (`_render_deadline`). A 500k-line backlog must
  still render in seconds — do not reintroduce unbounded full-list scans.
- **Python sidecars**: `webhook/`, `proxy/`, `mysql/`, `acoustid/`, `radio/`
  each have `server.py` + `Dockerfile` + `docker-compose.yml`. They must be
  Python 3.12-compatible (the base image), and response writes swallow
  `BrokenPipeError`/`ConnectionResetError` (a `_wfile_write` helper). Validate
  with `python -c "import ast; ast.parse(open('.../server.py').read())"`.

## Deployment model

- **Plugin `.ndp`**: built locally with `build.ps1`, copied to the Navidrome
  plugins share (`\\192.168.0.21\opt\navidrome\data\plugins\`), rescan plugins.
  Also built/published by GitHub Actions `release.yml` to a GitHub Release.
- **Sidecar images**: built + pushed to GHCR by `.github/workflows/docker.yml`
  (matrix: acoustid, webhook, proxy, mysql, radio). The NAS deploys via
  `docker-compose.yml` pulling `ghcr.io/.../sidecar:latest`. **If you change a
  sidecar's `server.py`, the container on the NAS must be redeployed** (pull +
  recreate) — a stale image is the usual cause of "the dashboard is old."
- Sidecars reach each other by container name on `stack_network` (external
  docker network that also includes Navidrome). Static container IPs break on
  recreate — prefer container names.
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