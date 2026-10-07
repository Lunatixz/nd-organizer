# HANDOFF — nd-organizer session state (2026-10-07, v3.1.0 force-fingerprint reparse deployed, dry pass running)

Read `AGENTS.md` first (repo rules: scope = this repo + 5 sidecar dirs only
`acoustid/ webhook/ proxy/ mysql/ essentia/`; Navidrome = logs + plugin config
edits only, NEVER Navidrome's own code).

## Deployed (2026-10-07 ~19:30 local) — v3.1.0, force-fingerprint reparse (dry review pending)

- **Feature**: confirmed AcoustID matches (score >= minConfidence) now fill
  placeholder track artists (missing/Unknown/Various Artists) in KV + file tags
  (`tags.rs artist_fill_candidate/fill_placeholder_artist`, verify step; also
  fixed `recordingId` vs `id` sidecar MBID key bug — mbid_recording was stored
  as ""). Album-level fixup: `organizer::collapse_album_artist` — placeholder/
  VA album artist + ALL tracks one real artist ⇒ KV + file album_artist patched
  at group_step (file tags first, KV after; ≤8s budget between groups, resumes
  next pass). Destination library ("Move to library") now plans + moves under
  `forceFingerprint`/`forceRefingerprintUnknownArtist` in BOTH modes (dry =
  would-move report): cleanup releases the meta-only stash as plan tasks;
  plan_move/plan_enrich meta-only gates updated (singles routing still
  disabled there; enrich keeps moves only when mode==apply). NFO now
  serializes `<artist>` list (write_group_nfo populates `artists`).
  Manifest descriptions updated for both force toggles + moveDestinationLibrary.
- **Tests**: 91/91 (new: artist_fill_candidate_gates, collapse_album_artist_rules;
  caught+fixed: collapse must BLOCK on still-unknown tracks, not filter them).
- **Config now**: mode=**dryRun**, forceRefingerprintUnknownArtist=**true**,
  forceFingerprint=false, moveDestinationLibrary=/music, playbackStatsEnabled=true
  (statsPollMinutes=5). Pass started after enable (walk lib1+lib2).
- **DO NOT flip mode to apply until the dry would-move report has been
  reviewed together** (blast radius: force fixes can rename many /music folders).
  Next: watch logs for "cleanup: released N stashed plan task(s)
  (force-fingerprint reparse)" + plan_move dry reports, review, then apply.
- **Webhook now-playing "not working" (2026-10-07)**: NOT a code bug — the
  plugin was left **disabled** (interrupted earlier deploy flow), so stats
  heartbeats stopped at 18:05 UTC and `latest_np()`'s 30-min freeze guard
  blanked the card. Re-enabled during deploy; UI verified showing live card
  (art/title/progress). If it blanks again with playback active: check
  `enabled` flag first, then webhook.log `phase":"stats` lines.

## Pass #5 result + cleanup truncation bug (2026-10-06 morning)

- **Pass #5 completed** (enrich → plan_move → cleanup overnight): lib1
  "372 sidecar folder(s) merged" (drain 06:09 UTC), lib2 "14 merged"
  (drain 09:34 UTC), 0 deleted. Watcher watch_pass2.py died 20:22 local
  (plugin fine — log timestamps are UTC, local = UTC-4).
- **Audit FAILED**: `/music/Gracie Abrams/The Secret of Us (Deluxe) (2024)`
  (audio-less, 3 sidecars) still twin next to `The Secret of Us` (13 flac).
- **Root cause**: `collect_dirs_bounded`'s 12s budget silently TRUNCATED the
  dir list; truncated list cached, drained, reported as complete. Merge log
  order proves it: forward-alpha collect cut right after "Foo Fighters",
  reversed → walk started at Foo, ended at 2 Chainz. **All artists after Foo
  (G–Z: Gracie, Gaga, Metallica, Radiohead, U2…) were never collected.**
  prefix_score would have matched Gracie (norm test already asserts the pair).
- **Fix (built, not deployed)**: `organizer::cleanup_walk` — resumable LIFO
  post-order cursor `(rel, entered)`, one stack persisted in KV
  (`cleanup.stack.{lib}`), 15s budget per chunk, ≥1 step per call (no
  stalls); `cleanup_step` is now thin glue (counters/logs/enqueue);
  `collect_dirs_bounded` deleted. Native tests:
  `cleanup_walk_merges_twins_across_the_whole_tree`,
  `cleanup_walk_resumes_across_slices_without_truncating` → **89/89**,
  wasm build clean.
- Also done this session: **corrupt DB deleted** (`navidrome.db.corrupt-20261002`,
  508.6 MB freed, user-approved).

## Deployed (2026-10-06 16:52 local) — v3.0.0, audit PASSED

`build.ps1 -Install` + container stop → delete corrupt `taskqueue.db` →
start + `PUT enabled` → **DEPLOY OK** (config 6/6, 0 new errors). Contents:
(1) meta_verify-first rewrite + lofty key fixes, (2) cleanup streaming walk
(`organizer::cleanup_walk`), (3) group_step reorder (cleanup enqueue above
enrich/plan), (4) meta-only enrich STASH: group_step stashes groups
(`enrich.stash.{lib}`), cleanup's done branch releases them — the
~1600-task per-album burst can never again sit ahead of cleanup's 15s-budget
continuation (it starved for hours pre-fix).

### Audit result (post-deploy pass)

- `cleanup: 838 sidecar folder(s) merged, 2 no-audio folder(s) deleted` —
  chunks ran back-to-back (20:46→20:57), zero starvation, G–Z region finally
  scanned (was truncated at 12s before).
- **Gracie Abrams = ONE folder** (`The Secret of Us`; the audio-less
  `(Deluxe) (2024)` twin merged away).
- Stash round-trip logged: `1597 group(s) stashed` → `released 1597
  stashed enrich task(s)` 3s after the completion summary.
- meta_refresh verify-first observed: dir-op (album.nfo) before per-file,
  `refreshed` << `processed` (gates), `deferred — organize pipeline active`
  guard, cursor resumable (lib1 at 4763/20430 when pipeline busy).
- Tests 89/89, wasm release build clean.

### taskqueue.db corruption episode (2026-10-06 afternoon)

taskqueue.db went `database disk image is malformed` (SQLite over SMB is
unsupported; external python writes from this machine finished it off —
**never write these DBs externally, reads were also risky**). Symptom: every
`dequeue` failed each 5s, cleanup starved behind a frozen 1443-task backlog.
Fix: queue is a regenerable cache (every kind re-enqueues per pass) → stop
container, delete `taskqueue.db`(+wal/shm), start. kvstore integrity was
`ok` and stayed (file index preserved — no re-index needed beyond the normal
per-pass validation).

## meta_refresh verify-first rewrite (2026-10-05 evening, BUILT, NOT deployed)

User directive: "we verify first than process if meta is missing. we can't
assume every track was processed by nd-org in the past." Implementation:

- `meta_refresh_step` (scan.rs) rewritten: per-file verify-first ops —
  ReplayGain (gate `replaygain_present`), genre (gate non-empty cur.genre,
  nfo_genres fallback), acoustic tags (gate `acoustic_missing`), essentia
  single-file (cap 8s, only when genre_source=essentia + still empty), MBID
  gap fill from index KV, instrumental/acoustic label checks on `cur_title`
  (no re-read). Album dir-op runs BEFORE per-file ops (once per dir per
  chunk): front art/sidecar/extras/album.nfo missing-only, 12s entry gate
  defers mid-chunk (cursor not advanced); counters art_written/nfo_written;
  post-loop `trigger_navidrome_scan` when refreshed>0.
- **lofty silent-write root cause**: `insert_text(ItemKey::Unknown(..))`
  returns false (re_map → allow_unknown=false → None), and reads via
  `get_string(&ItemKey::Unknown(..))` never match reader-stored known keys →
  ReplayGain/MOOD/ENERGY/BPM/KEY writes were NO-OPS claiming success. Fixed:
  new `tags::get_str_any` / `tags::insert_text_any` helpers; RG read/write
  now uses proper `ItemKey::ReplayGain*` keys; audiomuse write_tags + stats
  write_essentia_tags keys fixed (BPM=[Bpm,IntegerBpm], KEY=[InitialKey],
  MOOD=[Mood], ENERGY/CHORD/STRUCTURE=Unknown via insert_unchecked fallback).
  MP4 has no ReplayGain map (ceiling, commented). MBID keys map on all
  formats incl. MP4 ✓.
- `write_group_nfo` essentia body: genres/moods false→true (shared
  `essentia:{path}` 7d KV cache — NFO-only responses poisoned refresh reads).
- `write_essentia_genres` gained `timeout_ms` param (caller passes 20_000).
- README metadata-refresh bullet fixed (verify-first wording).
- Tests: `tags::verify_gate_tests` (2 new) → **87/87 green**, wasm build
  clean. scan.rs is wasm-only — its errors surface only in the wasm build.
- **Known unfixed same-class bug (out of scope, offer as follow-up)**:
  `write_playback_meta` (tags.rs:317, RATING/FMPS/LOVED via Unknown keys) is
  still a silent no-op; called only from stats `write_playback_meta_tags`.
- NOT implemented (deliberate): lyrics .lrc, AcoustID submit, ratings,
  NFO/artwork dedup markers (circuit breakers + 7d KV caches bound retries).

## Deployed (2026-10-05 ~10:55)

- **Build -Install + container restart + enable all done.** Config verified:
  enabled=true, mode=apply, variousFolder=Various Artists,
  useLidarrNamingSchema=true, moveDestinationLibrary=/music, 0 error lines.
- Deployed binary includes: singles ← Lidarr relabel, Lidarr naming fallback,
  MB year fallback, meta-only enrichment, Various Artists, AND the
  **duplicate-folder fix** (4 root-cause fixes, tests 85/85, wasm clean).
- **`cleanupNoAudioFolders` enabled 2026-10-05 11:1x** (user request) — tier-3
  plain-empty folders now deleted too; all other config fields verified intact.
- VA rename was a no-op: singular "Various Artist" already absent everywhere.
- Config backup: `plugin_config_backup_20261005_105157.json` (Temp\opencode).
- **Pass #5 started ~11:07 with the new binary** — currently walk phase.
  Watch via `watch3.log` (watch_pass2.py running, PID ~14932).

## The duplicate-folder fix (deployed in pass #5)

1. `scan.rs plan_enrich_step`: `sidecar_abs` guard — cover/nfo write next to the
   audio when the target folder doesn't hold files yet (was: creating
   sidecar-only twins in /music).
2. `organizer.rs apply_group_plan_to`: post-move sweep — leftover album sidecars
   (folder.jpg, cdart, nfo, release-named cue/sfv/nfo) follow the album out of
   emptied source dirs; exact-name dupes dropped.
3. `SIDECAR_EXTS` += sfv/log; `DIR_SIDECARS` += cdart.png; legacy `apply_plan`
   sidecar src→src no-op fixed.
4. `cleanup_step`: `merge_sidecar_twin` consolidates EXISTING audio-less twin
   folders into the best-matching audio sibling (normalized prefix ≥ 4 — handles
   curly quotes, "(2011)", "(Deluxe)"); always enqueued; deletion still gated
   by cleanupNoAudioFolders (default off). `manifest.json` description updated.

## Pass #5 / #6 success criteria — all met 2026-10-06

- `cleanup: merged <twin> -> <album>` lines — 838 merges this pass (Gracie
  Abrams, Foo Fighters, Fleetwood Mac, H₂O curly-quote twins, …).
- NO new sidecar-only dirs after enrich (sidecar_abs guard + post-move sweep).
- `naming schema` log line on first plan (useLidarrNamingSchema) ✓ earlier pass.
- Audio-less dirs audited gone: `twin_probe3.py` shows Gracie = ONE folder ✓.

## Remaining / watch items

1. lib2 chain overnight: verify lib2 (~19k left at 21:00) → group → cleanup
   lib2. Watch for cleanup lib2 `N dirs remaining` lines NOT completing —
   plan_move's per-album enrich burst can starve the NON-meta continuation
   the same way lib1's did (lib1 is fixed via the stash; source-library path
   is not). If stalled: same pattern applies — stash plan groups until
   cleanup done.
2. Enrich release: 1597 tasks draining (~10-14s each ≈ 5h). Artwork lookups
   that never succeed re-burst every pass — pre-existing convergence issue,
   offer as follow-up (needs a give-up marker).
3. Open item: delete `data/navidrome.db.corrupt-20261002` — needs user OK.
4. Known unfixed same-class bug (offer as follow-up): `write_playback_meta`
   (tags.rs:317) RATING/FMPS/LOVED via Unknown keys = silent no-op.

## Key facts

- Container: `audiomuse-navidrome-navidrome-1` (navidrome twice — typo source of
  failed deploys). Portainer `http://192.168.0.21:9000/api/endpoints/13/docker`,
  key + plugin API credentials + Lidarr key: in `deploy_v3.py` / AGENTS.md.
- Plugin API `http://192.168.0.21:4533`, header `X-ND-Authorization: Bearer <token>`.
- kv sqlite: `\\192.168.0.21\opt\navidrome\data\plugins\nd-organizer\kvstore.db`,
  table `kvstore` (columns `key`,`value`); file index key
  `scan.filev2.{lib}:{fnv1a64(rel):016x}`.
- Live config: mode=apply, `/unsorted` (lib 2) → `/music` (lib 1, meta-only),
  `variousFolder="Various Artists"`, `useLidarrNamingSchema=true`,
  `classifyFromMB=true`, `maxAlbumsPerRun=100`.
- Test/build: `cargo test` (89), `cargo build --release --target wasm32-wasip1`,
  deploy build: `pwsh scripts/build.ps1 -Install`.
- Pass #5/#6 status: cleanup done 20:57 (838 merges); enrich release 1597
  tasks draining from 20:57, ETA ~02:00 local; then next meta_refresh pass
  resumes cursor 4763/20430. Docker log timestamps are UTC (local = UTC-4);
  the plugin's own `storage/nd-organizer.log` logs [epoch] epoch seconds.
- Repo is CRLF: the edit tool often fails "oldString not found" on anchors —
  workaround is rewriting the whole file.
- scan.rs is `#[cfg(target_arch = "wasm32")] mod` (lib.rs) → NOT compiled by
  native `cargo test`; testable logic lives in organizer.rs on purpose.
