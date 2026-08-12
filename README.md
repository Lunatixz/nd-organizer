# nd-organizer

A [Navidrome](https://www.navidrome.org/) plugin (Rust → WebAssembly, packaged as
`.ndp`) that organizes your music library:

- **Classifies albums** into buckets and renames folders/files to schemas you
  define:
  - Soundtracks → `Various Artist/Sound Tracks/{album} ({year})`
  - Various-artist compilations → `Various Artist/{album} ({year})`
  - Singles / single songs / incomplete albums → `{albumArtist}/Singles/{title}`
    (under the artist; falls back to `Various Artist/Singles/...` when the
    artist is unknown)
  - Everything else → `{albumArtist}/{album} ({year})`
- **Preserves recording source**: live/bootleg tracks are detected (LIVE/BOOTLEG
  tags, genre, title markers) and suffixed with `(Live)`/`(Bootleg)` so they
  never collide with — or get mistaken for — the studio release.
- **Refreshes metadata + artwork** from external sources (MusicBrainz, Cover Art
  Archive, iTunes, Last.fm) with an **identity-verification gate** (AcoustID
  audio fingerprint + embedded tags + file/folder names + metadata analysis)
  before anything is written.
- **Integrates with Lidarr** (metadata/classification source for tracked
  artists; optional force-search for incomplete monitored albums) and
  **AudioMuse-AI** (acoustic BPM/key/mood/energy tags + re-sync after renames).
- **Multi-library**: organize one or many Navidrome libraries.
- **NFO sidecars**: reads Kodi-style `album.nfo`/`artist.nfo` to fill gaps, and
  can rewrite them after metadata is collected.
- **Triggers Navidrome rescans per album** whenever paths change, so the player
  never tries to stream an old filename.
- **Everything is logged** to the plugin folder (`nd-organizer.log` + per-run
  `report-*.txt`).

**Safety model:** `mode: dryRun` is the default. Every run writes a report to
the plugin storage dir (`.../plugins/nd-organizer/storage/`) and changes
nothing. Switch to `mode: apply` only after reviewing a dry-run report. Before
any tag/NFO write the plugin snapshots the previous tags/nfo (metadata only,
never audio bytes); `backupRetentionDays` (default 30) prunes old snapshots and
reports.

## Status (Phase 1 — skeleton)

Implemented now:

- Config schema + typed parsing (multi-library, classification, schemas,
  verification, metadata sources, Lidarr, AudioMuse-AI, scanning, safety)
- Schema template engine + filename sanitizer (`{track:02} - {title}`, `{album}`,
  `{year}`, `{recording}`, …) with user-tunable sanitize options
- Album discovery (hidden-file + exclude-path filtering), classification
  (Soundtrack > Various > Singles > Normal), rename-plan builder (files +
  sidecars + collision detection), apply (file/folder moves + empty-dir pruning)
- Recording-source detection (live/bootleg) with `(Live)`/`(Bootleg)` suffixing
- NFO sidecar read (fills metadata gaps) + write (updates after collection)
- Running log (`nd-organizer.log`) + per-run reports in plugin storage
- Capabilities: Lifecycle, Scheduler, TaskWorker (one album per task, concurrency 1)

Coming in later phases (already planned/config-shaped):

- Metadata clients (MusicBrainz, Cover Art Archive, iTunes, Last.fm, AcoustID)
- Identity verification (AcoustID fingerprint via symphonia+chromaprint, voting)
- Embedded tag + artwork writing (lofty), metadata backups
- Per-album `startScan` triggers
- Lidarr (tracked/untracked split, force-search incomplete) + AudioMuse-AI

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

The Navidrome Rust PDK (`nd-pdk` crates) is vendored under `pdk/` as path
dependencies so the plugin builds offline and against a pinned API.

## Install & configure

1. Copy `nd-organizer.ndp` into Navidrome's plugins folder
   (`<DataFolder>/plugins`, e.g. `\\192.168.0.21\opt\navidrome\data\plugins`).
2. In the Navidrome UI go to **Settings → Plugins**, rescan, enable
   **Music Organizer**, and grant library access.
3. **Grant filesystem write access** (required for renames/tag writes). There is
   no UI toggle for this — use the CLI on the Navidrome host:

   ```bash
   navidrome plugin edit nd-organizer --write-access --all-libraries
   ```

4. Configure in the UI:
   - **General**: leave `mode: dryRun` for now.
   - **Permissions (bottom of the plugin page / CLI)**: grant **Library Access**
     to the libraries you want organized — this is the selector, and it shows
     each library's path. Grant write access via
     `navidrome plugin edit nd-organizer --write-access`.
   - **Scanning**: set `scanUser` to a Navidrome admin username (needed for the
     per-album `startScan` once path changes are enabled in a later phase).
5. Trigger a pass by enabling `runOnStartup` (or restarting Navidrome, or
   setting a `scheduleCron`). Check `.../plugins/nd-organizer/storage/` for
   `nd-organizer.log` and `report-*.txt`, review the plan, then switch
   `mode: apply`.

The organizer processes every library the **Library Access** permission grants
(all of them by default). On startup and after each run, the log prints a
library inventory — each library's ID, name, path, and access flag
(`READ-WRITE` / `READ-ONLY` / `NO ACCESS`).

## Config reference

| Key | Default | Meaning |
|---|---|---|
| `mode` | `dryRun` | `dryRun` previews; `apply` writes. |
| Libraries | (permission) | Which libraries to organize = the Navidrome **Library Access** permission (lists library paths). Empty config organizes all granted. |
| `runOnStartup` / `scheduleCron` | `false` / `""` | How runs are triggered. |
| `folderSchema` | `{albumArtist}/{album} ({year})` | Album folder template. |
| `fileSchema` | `{track:02} - {title}` | Track file template (ext auto-added). |
| `soundtrackFolder`/`variousFolder`/`singlesFolder` | `Sound Tracks`/`Various Artist`/`Singles` | Bucket folder names. |
| `singlesUnderArtist` | `true` | `Artist/Singles/{title}` vs `Various Artist/Singles/...`. |
| `preserveRecordingType` | `true` | Append `(Live)`/`(Bootleg)` so live tracks stay distinct. |
| `excludePaths` | `[]` | Paths never to touch (e.g. `inbox`, `Downloads/*`). |
| `readNfo` / `writeNfo` | `true` / `false` | NFO sidecar read / rewrite. |
| `backupBeforeWrite` / `backupRetentionDays` | `true` / `30` | Metadata-only pre-write snapshots + retention. |
| `incompleteAlbumMinTracks` | `3` | Albums with fewer tracks route to Singles (until MB track-count data is available). |
| `verifyIdentity`, `minConfidence`, `skipUnverified`, `acoustidMode`, `acoustidApiKey` | `true`, `0.6`, `true`, `fingerprint`, — | Identity-verification gate (Phase 2). |
| Lidarr / AudioMuse / artwork / scan keys | — | See manifest + later phases. |

Template placeholders: `{track}` `{disc}` `{title}` `{artist}` `{albumArtist}`
`{album}` `{year}` `{genre}` `{recording}` `{mbid}`, plus `:NN` zero-padding
(`{track:02}`). Filenames are sanitized for all OSes (illegal chars replaced,
reserved Windows names guarded, length-capped).

## Security notes

- Library mounts are **read-only unless you run the CLI `--write-access` flag**.
  Only grant it to plugins you trust.
- `http.requiredHosts` is `["*"]` because Lidarr/AudioMuse URLs are
  user-configured per install (same precedent as `nd-lyrics`). The plugin still
  only makes outbound requests to hosts you put in its config.
- Plugin config (API keys) is stored in Navidrome's config store.

## Known limitations

- AudioMuse-AI acoustic features only exist for tracks it has already analyzed.
- AcoustID fingerprinting requires the in-sandbox audio pipeline (Phase 2).
- One album per background task (callbacks are time-limited); `maxConcurrency`
  is 1 so per-album rescans stay coherent.
- After renames, Navidrome favorites/playcounts survive only if **Persistent
  IDs** are enabled in Navidrome config.
