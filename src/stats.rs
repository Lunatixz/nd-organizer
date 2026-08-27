// Playback statistics + weighting.
//
// Tracks how often each song is PLAYED in full and how often it's SKIPPED, both
// observed from getNowPlaying transitions (works on older Navidrome that lacks
// the scrobbleretriever/users host services). A full play forgives a skip.
// Derives a weight and builds/updates a Navidrome playlist of the top picks so
// high-weight songs surface more often and frequently-skipped ones don't.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct NowPlayingEntry {
    pub id: String,
    pub position_ms: i64,
    pub duration: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub play_count: i64,
}

/// A track played for less than `threshold_pct`% of its duration counts as a skip.
pub fn is_skip(duration_sec: i64, position_ms: i64, threshold_pct: i32) -> bool {
    if duration_sec <= 0 {
        return false;
    }
    let played_pct = (position_ms as f64) / ((duration_sec as f64) * 1000.0) * 100.0;
    played_pct < (threshold_pct.max(0) as f64)
}

/// Weight a song by plays vs skips. Skipped plays drag it down twice as hard.
pub fn weight(plays: i64, skips: i64) -> f64 {
    plays as f64 - 2.0 * skips as f64
}

/// True when a song should be hard-removed from playback: skipped strictly more
/// than it was ever played in full (net negative), past the ratio cap, with
/// enough samples. Songs you like but occasionally skip never hit this - they
/// only sink in priority via weight reordering.
pub fn hard_exclude(plays: i64, skips: i64, ratio: f64, min_samples: i64) -> bool {
    let total = plays + skips;
    plays < skips
        && total >= min_samples
        && (skips as f64) / (total as f64) >= ratio.clamp(0.0, 1.0)
}

/// A 1-5 enjoyment rating from plays/skips (for future tag writes).
pub fn rating_1_5(plays: i64, skips: i64) -> u8 {
    if plays <= 0 {
        return 0;
    }
    let ratio = plays as f64 / ((plays + skips).max(1) as f64);
    let r = (1.0 + 4.0 * ratio).round().clamp(1.0, 5.0);
    r as u8
}

/// Parse a Subsonic `getNowPlaying` response.
pub fn parse_nowplaying(json: &str) -> Vec<NowPlayingEntry> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let entries = v
        .pointer("/subsonic-response/nowPlaying/entry")
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default();
    entries
        .into_iter()
        .filter_map(|e| {
            let id = e.get("id")?.as_str()?.to_string();
            Some(NowPlayingEntry {
                id,
                position_ms: e.get("positionMs").and_then(|x| x.as_i64()).unwrap_or(0),
                duration: e.get("duration").and_then(|x| x.as_i64()).unwrap_or(0),
                title: e.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                artist: e.get("artist").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                album: e.get("album").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                path: e.get("path").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                play_count: e.get("playCount").and_then(|x| x.as_i64()).unwrap_or(0),
            })
        })
        .collect()
}

/// Extract the playlist id from a Subsonic `createPlaylist` response.
pub fn parse_playlist_id(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    v.pointer("/subsonic-response/playlist/id")
        .and_then(|i| i.as_str())
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------- star rating

/// Per-file playback tally for the 0-5 star system. `full` is also the
/// playcount (only full listens >= starFullPlayPercent increment it).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct StarTally {
    /// Absolute library path + filename the tally belongs to.
    pub path: String,
    /// Last-known Navidrome media file id (for setRating / baseline reads).
    #[serde(default)]
    pub id: String,
    /// Title/artist captured at listen time (for display in dashboards).
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    /// Full listens (>= full%): +1.0 star AND +1 playcount.
    pub full: i64,
    /// Half listens (half%..full%): +0.5 star, no playcount.
    pub half: i64,
    /// Skips (< half%): -0.5 star penalty, no playcount.
    pub skips: i64,
    /// Loved/favorite flag (rating >= 3). Seeded from Last.fm on first sight.
    #[serde(default)]
    pub loved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StarBand {
    Ignore,
    Skip,
    Half,
    Full,
}

/// Classify a listen that ended at `played_pct` of its duration. Listens below
/// `ignore_pct` are Ignore (a momentary tap that never really played) - they
/// carry NO penalty and no credit, so they can't drag a rating down.
pub fn star_band(played_pct: f64, half_pct: i32, full_pct: i32, ignore_pct: i32) -> StarBand {
    if played_pct >= full_pct.max(half_pct) as f64 {
        StarBand::Full
    } else if played_pct >= half_pct as f64 {
        StarBand::Half
    } else if played_pct < ignore_pct as f64 {
        StarBand::Ignore
    } else {
        StarBand::Skip
    }
}

/// Apply one observed listen to a tally. A full listen also forgives one prior
/// skip (removes a pending -0.5 penalty), so sustained listening can recover a
/// rating. `full` is the playcount and is only ever incremented by Full.
/// `Ignore` returns the tally unchanged.
pub fn apply_band(mut t: StarTally, band: StarBand) -> StarTally {
    match band {
        StarBand::Full => {
            t.full += 1;
            if t.skips > 0 {
                t.skips -= 1;
            }
        }
        StarBand::Half => t.half += 1,
        StarBand::Skip => t.skips += 1,
        StarBand::Ignore => {}
    }
    t
}

/// Star credit in 0.5 steps: full +1.0, half +0.5, skip -0.5. Always clamped
/// to 0.0..5.0 so a rating can never go negative or exceed 5.
pub fn star_credit(t: &StarTally) -> f64 {
    (t.full as f64 + t.half as f64 * 0.5 - t.skips as f64 * 0.5).clamp(0.0, 5.0)
}

/// Seed a first-seen tally's initial rating from listening history:
/// a Last.fm "loved" track starts at a floor of 3.0 stars; a higher playcount
/// earns more starting stars (diminishing), capped. `full` already holds the
/// playcount baseline. This maps history to a fair starting point before the
/// user's own listening re-rates it.
pub fn seed_initial_rating(mut t: StarTally, playcount: i64, loved: bool) -> StarTally {
    // Loved floor: at least 3 stars.
    let mut credit: f64 = if loved { 3.0 } else { 0.0 };
    t.loved = loved;
    // Playcount rewards: every 5 plays adds a star step up to a 4.5 ceiling,
    // so a heavily-played but unloved track can still start high.
    let play_credit: f64 = (playcount as f64 / 5.0).min(4.5);
    credit = credit.max(play_credit);
    // Convert initial credit into half/full counts. full carries +1 each (and
    // is the playcount), so fold the whole-star part into `full` would inflate
    // playcount - instead express credit via `half` (0.5 each) only, leaving
    // `full` as the true playcount. We set half = round(credit / 0.5).
    t.half = (credit / 0.5).round() as i64;
    t.skips = 0;
    t
}

/// Seed a first-seen tally directly from an explicit external rating (e.g. a
/// Lidarr track/album rating, 0-5). Loved = rating >= 3.
pub fn seed_from_rating(mut t: StarTally, rating: f64) -> StarTally {
    let r = rating.clamp(0.0, 5.0);
    t.half = (r / 0.5).round() as i64;
    t.skips = 0;
    t.loved = r >= 3.0;
    t
}

/// 0-5.0 rating, half-star granularity, capped at 5.0.
pub fn star_rating(t: &StarTally) -> f64 {
    let c = star_credit(t).clamp(0.0, 5.0);
    (c * 2.0).round() / 2.0
}

/// One full stats pass result: per-track events observed this cycle plus
/// cumulative counters so the task log reads as a real, detailed report.
    pub struct StatsReport {
        pub plays: usize,
        pub skips: usize,
        pub events: Vec<String>,
        pub total_plays: i64,
        pub total_skips: i64,
        pub tracked: usize,
        /// What is currently playing (from the getNowPlaying snapshot).
        pub now_playing: Vec<NowPlayingEntry>,
    }

/// Human-readable summary of a stats pass for the task log / dashboard.
pub fn describe(r: &StatsReport, picks: usize, filtered: usize, ratings: usize, meta_writes: usize) -> String {
    let mut lines = vec![format!(
        "stats: {} play(s), {} skip(s) observed",
        r.plays, r.skips
    )];
    if r.events.is_empty() {
        lines.push("  no playback activity observed this cycle".into());
    } else {
        lines.extend(r.events.iter().map(|e| format!("  - {e}")));
    }
    lines.push(format!(
        "  cumulative: {} full plays, {} skips across {} tracked songs",
        r.total_plays, r.total_skips, r.tracked
    ));
    if picks > 0 {
        lines.push(format!("  Top Picks playlist refreshed with {picks} tracks"));
    }
        if filtered > 0 {
            lines.push(format!("  filter proxy: {filtered} skip-heavy track(s) removed"));
        }
        if ratings > 0 {
            lines.push(format!("  star: {ratings} rating(s) published to Navidrome"));
        }
        if meta_writes > 0 {
            lines.push(format!("  meta: wrote playcount/stars/loved to {meta_writes} track(s)"));
        }
        lines.join("\n")
    }

#[cfg(target_arch = "wasm32")]
pub mod host_stats {
    use crate::config::{Config, SkipContentMode};
    use nd_pdk::host;

    use super::*;

    fn play_key(mfid: &str) -> String {
        format!("stat.play.{mfid}")
    }
    fn skip_key(mfid: &str) -> String {
        format!("stat.skip.{mfid}")
    }
    fn now_key(user: &str) -> String {
        format!("stat.now.{user}")
    }

    fn bump(key: &str, delta: i64) {
        let cur: i64 = crate::store::kv().get(key)
            .ok()
            .flatten()
            .and_then(|v| String::from_utf8_lossy(&v).parse().ok())
            .unwrap_or(0);
        let _ = crate::store::kv().set(key, (cur + delta).max(0).to_string().into_bytes());
    }

    /// Observe now-playing transitions to estimate skips AND full plays, without
    /// needing the scrobbleretriever/users host services (missing on older
    /// Navidrome). A track that leaves playback between polls is:
    ///   - a SKIP if it stopped before `threshold_pct`% of its duration;
    ///   - a FULL PLAY otherwise - which also forgives one previous skip.
    /// getNowPlaying returns every active session, so a single pass covers all
    /// users. Plays from observations are incremental (no historical scrobble
    /// ingestion), so weights build up over time on older hosts.
    /// The same pass also feeds the 0-5 star tally (per filepath+filename).
    fn observe(cfg: &Config, user: &str) -> Result<StatsReport, String> {
        let uri = format!("getNowPlaying?u={user}");
        let json = host::subsonicapi::call(&uri).map_err(|e| e.to_string())?;
        let current = parse_nowplaying(&json);
        let key = now_key(user);
        let previous: Vec<NowPlayingEntry> = crate::store::kv().get(&key)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default();
        let mut plays = 0usize;
        let mut skips = 0usize;
        let mut events: Vec<String> = Vec::new();
        let current_ids: Vec<String> = current.iter().map(|e| e.id.clone()).collect();
        for prev in &previous {
            // A previously-playing track is no longer playing -> it ended.
            if !current_ids.contains(&prev.id) {
                let label = if prev.artist.is_empty() && prev.title.is_empty() {
                    format!("track {}", prev.id)
                } else {
                    format!("{} - {} [{}]", prev.artist, prev.title, prev.album)
                };
                let pct = if prev.duration > 0 {
                    (prev.position_ms as f64) / (prev.duration as f64 * 1000.0) * 100.0
                } else {
                    0.0
                };
                if is_skip(prev.duration, prev.position_ms, cfg.skip_threshold_percent) {
                    bump(&skip_key(&prev.id), 1);
                    skips += 1;
                    events.push(format!(
                        "skipped: {label} (stopped at {pct:.0}% of {}s)",
                        prev.duration
                    ));
                } else {
                    // Played through the skip threshold: full play + forgiveness.
                    bump(&play_key(&prev.id), 1);
                    bump(&skip_key(&prev.id), -1);
                    plays += 1;
                    events.push(format!("full play: {label}"));
                }
                // Star tally (0-5 rating) - independent of the filter weights.
                if cfg.star_tally_enabled && !prev.path.is_empty() {
                    record_star_listen(cfg, &prev, pct);
                }
            }
        }
        let _ = crate::store::kv().set(&key, serde_json::to_vec(&current).unwrap_or_default());
        let (total_plays, total_skips, tracked) = totals();
        Ok(StatsReport {
            plays,
            skips,
            events,
            total_plays,
            total_skips,
            tracked,
            now_playing: current,
        })
    }

    /// Highest-rated tracks (by star rating, then playcount) from the tally, for
    /// the dashboard's "playcounts & stars" view.
    pub fn top_rated(count: usize) -> Vec<(String, String, f64, i64)> {
        let mut rows = Vec::new();
        if let Ok(keys) = crate::store::kv().list("star.tally.") {
            for k in keys {
                let Ok(Some(v)) = crate::store::kv().get(&k) else { continue };
                let Ok(t) = serde_json::from_slice::<StarTally>(&v) else { continue };
                if t.full + t.half + t.skips <= 0 {
                    continue;
                }
                let name = if !t.title.is_empty() {
                    format!("{} - {}", t.artist, t.title)
                } else {
                    t.path.clone()
                };
                rows.push((name, t.path.clone(), star_rating(&t), t.full));
            }
        }
        rows.sort_by(|a, b| {
            b.2.partial_cmp(&a.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.3.cmp(&a.3))
        });
        rows.truncate(count.max(0));
        rows
    }

    /// Compute an album's rating. Sources, in priority order:
    ///   1. an external source passed in (Last.fm/MusicBrainz/Navidrome), or
    ///   2. the average of the album's track star ratings (from the tally),
    ///      keyed by the album directory (parent of each track's path).
    /// Returns None when no tracks in the album are rated and no external rating.
    pub fn album_rating_for(album_dir: &str, external: Option<f64>) -> Option<f64> {
        if let Some(r) = external {
            if r > 0.0 {
                return Some(r.clamp(0.0, 5.0));
            }
        }
        // Average the tracked tracks whose path sits under this album dir.
        let mut sum = 0.0f64;
        let mut n = 0usize;
        if let Ok(keys) = crate::store::kv().list("star.tally.") {
            for k in keys {
                let Ok(Some(v)) = crate::store::kv().get(&k) else { continue };
                let Ok(t) = serde_json::from_slice::<StarTally>(&v) else { continue };
                if t.full + t.half + t.skips <= 0 {
                    continue;
                }
                let dir = std::path::Path::new(&t.path)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                if dir.eq_ignore_ascii_case(album_dir) {
                    sum += star_rating(&t);
                    n += 1;
                }
            }
        }
        if n == 0 {
            None
        } else {
            Some((sum / n as f64).clamp(0.0, 5.0))
        }
    }

    fn star_tally_key(path: &str) -> String {
        format!("star.tally.{:016x}", crate::state::fnv1a64(path))
    }

    fn load_star(path: &str) -> Option<StarTally> {
        crate::store::kv()
            .get(&star_tally_key(path))
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice::<StarTally>(&v).ok())
            .filter(|t| !t.path.is_empty())
    }

    fn save_star(t: &StarTally) {
        let _ = crate::store::kv().set(
            &star_tally_key(&t.path),
            serde_json::to_vec(t).unwrap_or_default(),
        );
    }

    /// Apply one observed listen to the star tally. Seeds the baseline once per
    /// file from Navidrome's playCount (plus Last.fm when opted in), scrobbles
    /// full listens to Last.fm when enabled, and records the star band.
    fn record_star_listen(cfg: &Config, prev: &NowPlayingEntry, played_pct: f64) {
        let mut t = load_star(&prev.path).unwrap_or_else(|| StarTally {
            path: prev.path.clone(),
            id: prev.id.clone(),
            ..Default::default()
        });
        // Keep display info fresh so dashboards can show names without lookups.
        if !prev.title.is_empty() {
            t.title = prev.title.clone();
        }
        if !prev.artist.is_empty() {
            t.artist = prev.artist.clone();
        }
        t.id = if t.id.is_empty() { prev.id.clone() } else { t.id.clone() };
        // First sight of this file: seed the playcount baseline and an initial
        // rating that rewards history (higher playcounts -> higher starting
        // stars; a Last.fm "loved" track starts at 3; a Lidarr track/album
        // rating carries over directly).
        if t.full + t.half + t.skips == 0 {
            t.id = if t.id.is_empty() { prev.id.clone() } else { t.id.clone() };
            let mut baseline = prev.play_count.max(0);
            let mut loved = false;
            // External rating sources: Last.fm (playcount + loved), Lidarr
            // (track/album rating). The highest concrete rating wins the seed.
            let mut ext_rating: Option<f64> = None;
            if !prev.artist.is_empty() && !prev.title.is_empty() {
                if cfg.lastfm_import_playcount {
                    baseline = baseline.max(crate::favorites::host_favorites::playcount(
                        cfg,
                        &prev.artist,
                        &prev.title,
                    ));
                }
                if cfg.lastfm_import_playcount || cfg.favorites_sync_lastfm {
                    loved = crate::favorites::host_favorites::is_loved(
                        cfg,
                        &prev.artist,
                        &prev.title,
                    );
                }
                if !cfg.lidarr_url.trim().is_empty() {
                    // Prefer the track rating, fall back to the album rating.
                    ext_rating = crate::lidarr::host_lidarr::track_rating(
                        cfg,
                        &prev.album,
                        &prev.artist,
                        &prev.title,
                    )
                    .or_else(|| {
                        crate::lidarr::host_lidarr::album_rating(
                            cfg,
                            &prev.album,
                            &prev.artist,
                        )
                    });
                }
            }
            if baseline > t.full {
                t.full = baseline;
            }
            // Seed from the strongest signal: an explicit Lidarr rating wins;
            // otherwise use the playcount/loved mapping.
            t = match ext_rating {
                Some(r) => seed_from_rating(t, r),
                None => seed_initial_rating(t, baseline, loved),
            };
        }
        let band = star_band(
            played_pct,
            cfg.star_half_play_percent,
            cfg.star_full_play_percent,
            cfg.star_ignore_percent,
        );
        if band == StarBand::Full && cfg.lastfm_scrobble {
            crate::favorites::host_favorites::scrobble(
                cfg,
                &prev.artist,
                &prev.title,
                &prev.album,
                crate::state::now_ts(),
            );
        }
        if band == StarBand::Full && cfg.listenbrainz_scrobble {
            crate::favorites::host_favorites::listenbrainz_scrobble(
                cfg,
                &prev.artist,
                &prev.title,
                &prev.album,
                crate::state::now_ts(),
            );
        }
        if band == StarBand::Full && cfg.librefm_scrobble {
            crate::librefm::host_librefm::scrobble(
                cfg,
                &prev.artist,
                &prev.title,
                &prev.album,
                crate::state::now_ts(),
            );
        }
        let before = star_rating(&t);
        t = apply_band(t, band);
        let after = star_rating(&t);
        if before != after {
            crate::wasm::log_info(&format!(
                "star: {} - {} [{:?}] {before} -> {after} stars (full={} half={} skips={})",
                prev.artist, prev.title, prev.album, t.full, t.half, t.skips
            ));
        }
        save_star(&t);
    }

    /// Current rating + playcount for a file (None if untracked).
    pub fn star_summary(abs_path: &str) -> Option<(f64, i64)> {
        let t = load_star(abs_path)?;
        Some((star_rating(&t), t.full))
    }

    /// Re-key a tally when the organizer moves a file (old abs -> new abs),
    /// carrying the published-rating cache along so the rating survives.
    pub fn migrate_star_tally(from_abs: &str, to_abs: &str) {
        if from_abs == to_abs || from_abs.is_empty() || to_abs.is_empty() {
            return;
        }
        let Some(mut t) = load_star(from_abs) else { return };
        t.path = to_abs.to_string();
        save_star(&t);
        let _ = crate::store::kv().delete(&star_tally_key(from_abs));
        let fk = format!("star.pub.{:016x}", crate::state::fnv1a64(from_abs));
        let tk = format!("star.pub.{:016x}", crate::state::fnv1a64(to_abs));
        if let Ok(Some(v)) = crate::store::kv().get(&fk) {
            let _ = crate::store::kv().set(&tk, v);
            let _ = crate::store::kv().delete(&fk);
        }
    }

    /// Drop tallies whose file no longer exists on disk. Returns removed count.
    pub fn prune_star_tallies() -> usize {
        let mut removed = 0;
        if let Ok(keys) = crate::store::kv().list("star.tally.") {
            for k in keys {
                let Ok(Some(v)) = crate::store::kv().get(&k) else { continue };
                let Ok(t) = serde_json::from_slice::<StarTally>(&v) else { continue };
                if t.path.is_empty() {
                    continue;
                }
                if std::path::Path::new(&t.path).exists() {
                    continue;
                }
                let _ = crate::store::kv().delete(&k);
                removed += 1;
            }
        }
        removed
    }

    /// Publish computed star ratings to Navidrome's native rating (setRating).
    /// Apply mode only; only tracks with >= starMinSamples listens; only when
    /// the published value would change (cached per file, capped per pass).
    pub fn publish_star_ratings(cfg: &Config) -> Result<usize, String> {
        use crate::config::Mode;
        if !cfg.star_tally_enabled || cfg.mode != Mode::Apply {
            return Ok(0);
        }
        let user = crate::wasm::scan_user(cfg);
        if user.is_empty() {
            return Ok(0);
        }
        let mut published = 0usize;
        let mut loved_ops = 0usize;
        if let Ok(keys) = crate::store::kv().list("star.tally.") {
            for k in keys {
                if published >= 250 {
                    break; // cap per pass; the rest publish on later passes
                }
                let Ok(Some(v)) = crate::store::kv().get(&k) else { continue };
                let Ok(mut t) = serde_json::from_slice::<StarTally>(&v) else { continue };
                if t.id.is_empty() || t.path.is_empty() {
                    continue;
                }
                if t.full + t.half + t.skips < cfg.star_min_samples as i64 {
                    continue;
                }
                let stars = star_rating(&t);
                let int_rating = stars.round() as u8;
                let pub_key = format!("star.pub.{:016x}", crate::state::fnv1a64(&t.path));
                if let Ok(Some(prev)) = crate::store::kv().get(&pub_key) {
                    if String::from_utf8_lossy(&prev) == int_rating.to_string() {
                        continue;
                    }
                }
                let uri = format!("setRating?id={}&rating={}&u={user}", t.id, int_rating);
                match host::subsonicapi::call(&uri) {
                    Ok(_) => {
                        let _ = crate::store::kv().set(&pub_key, int_rating.to_string().into_bytes());
                        published += 1;
                    }
                    Err(e) => crate::wasm::log_warn(&format!(
                        "setRating {} ({}): {e}",
                        t.path, t.id
                    )),
                }
                // Loved = rating >= threshold (default 3): star (favorite) tracks
                // at/above it, unstar those below, so Navidrome's heart matches.
                let should_love = stars >= cfg.loved_threshold_stars;
                if t.loved != should_love {
                    let op = if should_love { "star" } else { "unstar" };
                    let love_uri = format!("{op}?id={}&u={user}", t.id);
                    match host::subsonicapi::call(&love_uri) {
                        Ok(_) => {
                            t.loved = should_love;
                            let _ = save_star(&t);
                            loved_ops += 1;
                        }
                        Err(e) => crate::wasm::log_warn(&format!(
                            "{op} {} ({}): {e}",
                            t.path, t.id
                        )),
                    }
                }
            }
        }
        // Push ratings to Lidarr (album + track) when enabled. Only for
        // Lidarr-tracked artists; best-effort, never fails the run.
        if cfg.rating_sync_write_to_lidarr
            && !cfg.lidarr_url.trim().is_empty()
            && !cfg.lidarr_api_key.trim().is_empty()
        {
            let mut lidarr_album_ops = 0usize;
            let mut lidarr_track_ops = 0usize;
            if let Ok(keys) = crate::store::kv().list("star.tally.") {
                for k in keys {
                    if lidarr_track_ops >= 250 {
                        break;
                    }
                    let Ok(Some(v)) = crate::store::kv().get(&k) else { continue };
                    let Ok(t) = serde_json::from_slice::<StarTally>(&v) else { continue };
                    if t.id.is_empty() || t.path.is_empty() {
                        continue;
                    }
                    if t.full + t.half + t.skips < cfg.star_min_samples as i64 {
                        continue;
                    }
                    let stars = star_rating(&t);
                    if stars <= 0.0 {
                        continue;
                    }
                    // Push track rating.
                    if !t.artist.is_empty() && !t.title.is_empty() {
                        // Extract album name from the path (parent dir name).
                        let album_name = std::path::Path::new(&t.path)
                            .parent()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        if !album_name.is_empty() {
                            match crate::lidarr::host_lidarr::set_track_rating(
                                cfg, &album_name, &t.artist, &t.title, stars,
                            ) {
                                Ok(Some(_)) => lidarr_track_ops += 1,
                                Ok(None) => {} // track not in Lidarr
                                Err(e) => crate::wasm::log_warn(&format!(
                                    "Lidarr track setRating {} - {}: {e}",
                                    t.artist, t.title
                                )),
                            }
                        }
                    }
                    // Push album rating (average of track ratings in this album).
                    let album_dir = std::path::Path::new(&t.path)
                        .parent()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if !album_dir.is_empty() && !t.artist.is_empty() {
                        let album_name = std::path::Path::new(&album_dir)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        if !album_name.is_empty() {
                            if let Some(album_stars) = album_rating_for(&album_dir, None) {
                                if let Some(album) = crate::lidarr::host_lidarr::find_album(
                                    cfg, &album_name, &t.artist,
                                ) {
                                    match crate::lidarr::host_lidarr::set_album_rating(
                                        cfg, album.id, album_stars,
                                    ) {
                                        Ok(()) => lidarr_album_ops += 1,
                                        Err(e) => crate::wasm::log_warn(&format!(
                                            "Lidarr album setRating {} - {}: {e}",
                                            t.artist, album_name
                                        )),
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if lidarr_album_ops > 0 || lidarr_track_ops > 0 {
                crate::wasm::log_info(&format!(
                    "star: pushed {lidarr_track_ops} track(s), {lidarr_album_ops} album(s) to Lidarr"
                ));
            }
        }
        // Push ratings to ListenBrainz (track, album, artist) when enabled.
        // Reads MBIDs from the file's tags to submit accurate rankings.
        if cfg.listenbrainz_scrobble && !cfg.musicbrainz_token.trim().is_empty() {
            let mut lb_ops = 0usize;
            if let Ok(keys) = crate::store::kv().list("star.tally.") {
                for k in keys {
                    if lb_ops >= 250 {
                        break;
                    }
                    let Ok(Some(v)) = crate::store::kv().get(&k) else { continue };
                    let Ok(t) = serde_json::from_slice::<StarTally>(&v) else { continue };
                    if t.id.is_empty() || t.path.is_empty() {
                        continue;
                    }
                    if t.full + t.half + t.skips < cfg.star_min_samples as i64 {
                        continue;
                    }
                    let stars = star_rating(&t);
                    if stars <= 0.0 {
                        continue;
                    }
                    // Read MBIDs from the file's tags.
                    if let Ok(tagged) = lofty::read_from_path(&t.path) {
                        use lofty::prelude::*;
                        if let Some(tag) = tagged.primary_tag() {
                            // Track rating (recording MBID)
                            if let Some(rec_mbid) = tag.get_string(&ItemKey::MusicBrainzRecordingId) {
                                if !rec_mbid.is_empty() {
                                    crate::favorites::host_favorites::listenbrainz_rate_track(
                                        cfg, rec_mbid, stars,
                                    );
                                    lb_ops += 1;
                                }
                            }
                            // Album rating: look up release_group_mbid from
                            // MusicBrainz cache, then rate on ListenBrainz.
                            if let Some(rel_mbid) = tag.get_string(&ItemKey::MusicBrainzReleaseId) {
                                if let Some(rg_mbid) = crate::musicbrainz::release_group_for_release(rel_mbid) {
                                    crate::favorites::host_favorites::listenbrainz_rate_album(
                                        cfg, &rg_mbid, stars,
                                    );
                                    lb_ops += 1;
                                }
                            }
                            // Artist rating (artist MBID)
                            if let Some(artist_mbid) = tag.get_string(&ItemKey::MusicBrainzArtistId) {
                                if !artist_mbid.is_empty() {
                                    crate::favorites::host_favorites::listenbrainz_rate_artist(
                                        cfg, artist_mbid, stars,
                                    );
                                    lb_ops += 1;
                                }
                            }
                        }
                    }
                }
            }
            if lb_ops > 0 {
                crate::wasm::log_info(&format!(
                    "star: pushed {lb_ops} rating(s) to ListenBrainz"
                ));
            }
        }
        if published > 0 || loved_ops > 0 {
            crate::wasm::log_info(&format!(
                "star: published {published} rating(s), {loved_ops} loved-status change(s) to Navidrome"
            ));
        }
        Ok(published)
    }

    /// Pull star ratings from Navidrome (setRating) into the plugin DB.
    /// Reads getStarred2 to find rated/loved tracks, then seeds the plugin's
    /// StarTally with the external rating when no local data exists yet.
    /// Useful when ratings were set manually in Navidrome's UI or another
    /// Subsonic client. Returns how many tracks were seeded.
    pub fn pull_navidrome_ratings(cfg: &Config) -> Result<usize, String> {
        use crate::config::Mode;
        if !cfg.rating_sync_pull_from_navidrome || cfg.mode != Mode::Apply {
            return Ok(0);
        }
        let user = crate::wasm::scan_user(cfg);
        if user.is_empty() {
            return Ok(0);
        }
        let uri = format!("getStarred2?u={user}");
        let json = host::subsonicapi::call(&uri).map_err(|e| e.to_string())?;
        let songs = crate::favorites::parse_starred(&json);
        let mut seeded = 0usize;
        for song in &songs {
            // Find the file by its Navidrome id (we need the path to look up
            // the tally). The starred list doesn't include the path, so we
            // search for it via search3.
            if song.title.is_empty() || song.artist.is_empty() {
                continue;
            }
            let search_uri = format!(
                "search3?query={}&songCount=1&u={user}",
                crate::favorites::host_favorites::urlencode(&format!("{} {}", song.artist, song.title))
            );
            let search_json = match host::subsonicapi::call(&search_uri) {
                Ok(j) => j,
                Err(_) => continue,
            };
            let results = crate::favorites::parse_starred(&search_json);
            let found = results.iter().find(|s| crate::favorites::same_track(
                &song.title, &song.artist, &song.mbid, s,
            ));
            // We need the file path from the song id — look it up via
            // getSong with the id to get the path.
            let song_id = match found {
                Some(s) if !s.id.is_empty() => &s.id,
                _ => continue,
            };
            let song_uri = format!("getSong?id={song_id}&u={user}");
            let song_json = match host::subsonicapi::call(&song_uri) {
                Ok(j) => j,
                Err(_) => continue,
            };
            let path = extract_path_from_song(&song_json);
            let Some(path) = path else { continue };
            // Only seed when the plugin has no local data yet.
            let existing = load_star(&path);
            if existing.is_some() {
                continue;
            }
            let mut t = StarTally {
                path: path.clone(),
                id: song_id.to_string(),
                title: song.title.clone(),
                artist: song.artist.clone(),
                ..Default::default()
            };
            // Check if this song has a rating (star/unstar implies loved, but
            // getStarred2 doesn't carry the numeric rating — we treat starred
            // as loved = 3 stars baseline).
            t.loved = true;
            t = seed_initial_rating(t, 0, true);
            save_star(&t);
            seeded += 1;
        }
        if seeded > 0 {
            crate::wasm::log_info(&format!(
                "star: pulled {seeded} rating(s) from Navidrome into plugin DB"
            ));
        }
        // Pull loved/hated feedback from ListenBrainz for tracks not yet seeded.
        if !cfg.musicbrainz_token.trim().is_empty() {
            let lb_feedback = crate::favorites::host_favorites::listenbrainz_get_feedback(cfg);
            if !lb_feedback.is_empty() {
                let mut lb_seeded = 0usize;
                if let Ok(keys) = crate::store::kv().list("star.tally.") {
                    // Find existing tallies by MBID to match ListenBrainz feedback.
                    for k in &keys {
                        let Ok(Some(v)) = crate::store::kv().get(k) else { continue };
                        let Ok(t) = serde_json::from_slice::<StarTally>(&v) else { continue };
                        // Already has local data — skip.
                        if t.full + t.half + t.skips > 0 {
                            continue;
                        }
                        // Read MBID from file tags.
                        if let Ok(tagged) = lofty::read_from_path(&t.path) {
                            use lofty::prelude::*;
                            if let Some(tag) = tagged.primary_tag() {
                                if let Some(rec_mbid) = tag.get_string(&ItemKey::MusicBrainzRecordingId) {
                                    if let Some(&score) = lb_feedback.get(rec_mbid) {
                                        let mut t2 = t.clone();
                                        if score == 1 {
                                            t2.loved = true;
                                        }
                                        let full = t2.full;
                                        let loved = t2.loved;
                                        t2 = seed_initial_rating(t2, full, loved);
                                        save_star(&t2);
                                        lb_seeded += 1;
                                    }
                                }
                            }
                        }
                    }
                }
                if lb_seeded > 0 {
                    crate::wasm::log_info(&format!(
                        "star: seeded {lb_seeded} rating(s) from ListenBrainz feedback"
                    ));
                }
            }
        }
        Ok(seeded)
    }

    fn extract_path_from_song(json: &str) -> Option<String> {
        let v: Value = serde_json::from_str(json).ok()?;
        v.pointer("/subsonic-response/song/path")
            .and_then(|p| p.as_str())
            .map(|s| s.to_string())
    }

    /// Cumulative play/skip counters + how many distinct songs are tracked.
    fn totals() -> (i64, i64, usize) {
        let mut plays = 0i64;
        let mut tracked = 0usize;
        if let Ok(keys) = crate::store::kv().list("stat.play.") {
            tracked = keys.len();
            for k in keys {
                if let Ok(Some(v)) = crate::store::kv().get(&k) {
                    plays += String::from_utf8_lossy(&v).parse::<i64>().unwrap_or(0);
                }
            }
        }
        let mut skips = 0i64;
        if let Ok(keys) = crate::store::kv().list("stat.skip.") {
            for k in keys {
                if let Ok(Some(v)) = crate::store::kv().get(&k) {
                    skips += String::from_utf8_lossy(&v).parse::<i64>().unwrap_or(0);
                }
            }
        }
        (plays, skips, tracked)
    }

    fn all_weights() -> Vec<(String, f64, i64, i64)> {
        let mut weights = Vec::new();
        if let Ok(keys) = crate::store::kv().list("stat.play.") {
            let vals = crate::store::kv().get_many(keys).unwrap_or_default();
            for (k, v) in vals {
                let mfid = k.strip_prefix("stat.play.").unwrap_or(&k).to_string();
                let plays = String::from_utf8_lossy(&v).parse::<i64>().unwrap_or(0);
                let skips = crate::store::kv().get(&skip_key(&mfid))
                    .ok()
                    .flatten()
                    .and_then(|v| String::from_utf8_lossy(&v).parse().ok())
                    .unwrap_or(0);
                weights.push((mfid, weight(plays, skips), plays, skips));
            }
        }
        weights.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        weights
    }

    /// Write playback metadata (playcount, star rating, loved status) into each
    /// tracked file's tags. Opt-in via `writePlaycount`, apply mode only. Loved
    /// is derived from the rating (>= 3 stars), per the "loved = 3+ stars" rule.
    pub fn write_playback_meta_tags(cfg: &Config) -> Result<usize, String> {
        use crate::config::Mode;
        if !cfg.write_playcount || cfg.mode != Mode::Apply {
            return Ok(0);
        }
        let mut written = 0usize;
        if let Ok(keys) = crate::store::kv().list("star.tally.") {
            for k in keys {
                let Ok(Some(v)) = crate::store::kv().get(&k) else { continue };
                let Ok(t) = serde_json::from_slice::<StarTally>(&v) else { continue };
                if t.path.is_empty() || !std::path::Path::new(&t.path).exists() {
                    continue;
                }
                let total = t.full + t.half + t.skips;
                let stars = if total >= cfg.star_min_samples as i64 {
                    Some(star_rating(&t))
                } else {
                    None
                };
                // Loved = rating >= threshold (default 3).
                let loved = stars.map(|s| s >= cfg.loved_threshold_stars);
                let abs = t.path.clone();
                let artist = t.artist.clone();
                if !crate::wasm::should_write_tags(cfg, &artist) {
                    continue; // Lidarr-tracked artist, writeTagsForTracked off
                }
                if cfg.backup_before_write {
                    if let Some(tags) = crate::tags::read_tags(std::path::Path::new(&abs)) {
                        let _ = crate::state::backup_tag_state(
                            &crate::state::host_state::new_run_id(),
                            &abs,
                            &tags,
                        );
                    }
                }
                match crate::tags::write_playback_meta(
                    std::path::Path::new(&abs),
                    stars,
                    t.full,
                    loved,
                    cfg.overwrite_existing_tags,
                ) {
                    Ok(()) => written += 1,
                    Err(e) => crate::wasm::log_warn(&format!("write playback meta {abs}: {e}")),
                }
            }
        }
        Ok(written)
    }

    /// Rebuild the "nd-organizer: Top Picks" Navidrome playlist from the weights.
    pub fn refresh_top_picks(cfg: &Config, count: usize) -> Result<usize, String> {
        let user = crate::wasm::scan_user(cfg);
        if user.is_empty() {
            return Err("no Navidrome user available (grant one in User Access) for Top Picks playlist".into());
        }
        let weights = all_weights();
        let top: Vec<(String, f64)> = weights
            .into_iter()
            .take(count)
            .map(|(m, w, _, _)| (m, w))
            .collect();
        if top.is_empty() {
            return Ok(0);
        }
        let mut q = format!(
            "createPlaylist?name={}&u={user}",
            urlencode("nd-organizer: Top Picks")
        );
        for (mfid, _) in &top {
            q.push_str(&format!("&songId={}", urlencode(mfid)));
        }
        // Update an existing playlist if we've created one before.
        if let Some(id) = crate::store::kv().get("stat.playlist.id").ok().flatten() {
            if let Ok(id) = String::from_utf8(id) {
                if !id.is_empty() {
                    q.push_str(&format!("&playlistId={}", urlencode(&id)));
                }
            }
        }
        let resp = host::subsonicapi::call(&q).map_err(|e| e.to_string())?;
        if let Some(pid) = parse_playlist_id(&resp) {
            let _ = crate::store::kv().set("stat.playlist.id", pid.into_bytes());
        }
        Ok(top.len())
    }

    fn urlencode(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{:02X}", b)),
            }
        }
        out
    }

    /// One stats pass: observe full plays + skips from now-playing transitions.
    /// No scrobbleretriever/users host services needed (older Navidrome).
    pub fn poll(cfg: &Config) -> Result<StatsReport, String> {
        let user = crate::wasm::scan_user(cfg);
        if user.is_empty() {
            return Ok(StatsReport {
                plays: 0,
                skips: 0,
                events: vec![],
                total_plays: 0,
                total_skips: 0,
                tracked: 0,
                now_playing: vec![],
            });
        }
        observe(cfg, &user)
    }

    /// Publish play/skip weights + skip-heavy track IDs + keywords to the
    /// Subsonic filter proxy. The proxy re-sorts returned song lists by weight
    /// (skipped tracks sink) and limits how many skip-heavy tracks can be queued
    /// per the configured mode (exclude/third/lessThanHalf/half). No files moved.
    /// Needs apply mode + filterUrl; runs when keyword filtering or skip-content
    /// limiting is enabled.
    pub fn publish_filters(cfg: &Config) -> Result<usize, String> {
        use crate::config::Mode;
        use std::collections::HashMap;
        if cfg.mode != Mode::Apply {
            return Ok(0);
        }
        let base = cfg.filter_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return Ok(0);
        }
        let kw_on = cfg.keyword_filter_enabled;
        let mode = cfg.skip_content_mode;
        if !kw_on && mode == SkipContentMode::None {
            return Ok(0);
        }
        let ratio = cfg.skip_heavy_ratio.clamp(0.0, 1.0);
        const MIN_SAMPLES: i64 = 3;
        let all = all_weights();
        // Skip-heavy = NET NEGATIVE: skipped strictly more often than ever played
        // in full (full plays forgive skips), 3+ interactions, and a skip
        // fraction at/above skipHeavyRatio. Only computed when the user wants
        // skip-content limiting at all.
        let excluded: Vec<String> = if mode == SkipContentMode::None {
            vec![]
        } else {
            all.iter()
                .filter(|(_, _, plays, skips)| hard_exclude(*plays, *skips, ratio, MIN_SAMPLES))
                .map(|(mfid, _, _, _)| mfid.clone())
                .collect()
        };
        let weights: Vec<serde_json::Value> = all
            .into_iter()
            .map(|(mfid, w, plays, skips)| serde_json::json!([mfid, w, plays, skips]))
            .collect();
        // Push the Navidrome fillerKeywords setting so it drives the proxy's
        // queue filtering (single source of truth; FILTER_KEYWORDS env is just
        // the startup default).
        let keywords = if kw_on {
            crate::organizer::filler_keyword_list(cfg)
        } else {
            Vec::new()
        };
        let payload = serde_json::json!({
            "excluded": excluded,
            "weights": weights,
            "keywords": keywords,
            "keywordFilter": kw_on,
            "skipMode": mode.as_str(),
        })
        .to_string();
        let req = host::http::HTTPRequest {
            method: "POST".into(),
            url: format!("{base}/filters"),
            headers: HashMap::from([("Content-Type".into(), "application/json".into())]),
            no_follow_redirects: false,
            body: payload.into_bytes(),
            timeout_ms: 15_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if (200..300).contains(&resp.status_code) => {
                crate::wasm::log_info(&format!(
                    "published {} skip-heavy flags + {} weights + {} keywords (mode {}) to filter proxy at {base}",
                    excluded.len(),
                    weights.len(),
                    keywords.len(),
                    mode.as_str()
                ));
                Ok(excluded.len())
            }
            Ok(Some(resp)) => Err(format!(
                "filter proxy {base} responded {}",
                resp.status_code
            )),
            Ok(None) => Err(format!("filter proxy {base} unreachable")),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Fetch genre/mood predictions from the Essentia sidecar and write them
    /// to each track's GENRE and MOOD tags. Called when genreFrom == "essentia".
    /// Also writes mood as a fallback when AudioMuse didn't provide it.
    /// Returns the count of files whose tags were actually modified.
    pub fn write_essentia_genres(
        cfg: &crate::config::Config,
        root: &std::path::Path,
        _plan: &crate::organizer::GroupPlan,
        files: &[(String, crate::tags::TrackTags)],
    ) -> usize {
        let base = cfg.essentia_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return 0;
        }
        let mut written = 0usize;
        for (rel, _ft) in files {
            let abs = root.join(rel);
            let path_str = abs.to_string_lossy().to_string();
            let cache_key = format!("essentia:{}", path_str);
            // Check cache first (7-day TTL).
            if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&v) {
                    if write_essentia_tags(&abs, &val, cfg.overwrite_existing_tags) {
                        written += 1;
                    }
                }
                continue;
            }
            let body = serde_json::json!({"path": &path_str, "genres": true, "moods": true});
            let mut headers = std::collections::HashMap::new();
            headers.insert("Content-Type".into(), "application/json".into());
            let req = host::http::HTTPRequest {
                method: "POST".into(),
                url: format!("{base}/analyze"),
                headers,
                no_follow_redirects: false,
                body: body.to_string().into_bytes(),
                timeout_ms: 20_000,
            };
            match host::http::send(req) {
                Ok(Some(resp)) if resp.status_code == 200 => {
                    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&resp.body) {
                        let _ = crate::store::kv().set_with_ttl(
                            &cache_key,
                            serde_json::to_vec(&val).unwrap_or_default(),
                            7 * 24 * 3600,
                        );
                        if write_essentia_tags(&abs, &val, cfg.overwrite_existing_tags) {
                            written += 1;
                        }
                    }
                }
                _ => {}
            }
        }
        written
    }
}

/// Write Essentia-predicted genres and mood to a file's tags. Returns true
/// when at least one tag was modified.
fn write_essentia_tags(path: &std::path::Path, data: &serde_json::Value, overwrite: bool) -> bool {
    use lofty::prelude::*;
    let Ok(mut tagged) = lofty::read_from_path(path) else {
        return false;
    };
    let mut tag = match tagged.primary_tag() {
        Some(t) => t.to_owned(),
        None => return false,
    };
    let mut changed = false;
    // Genres: semicolon-separated from Essentia's top predictions.
    if let Some(genres) = data.get("genres").and_then(|g| g.as_array()) {
        let genre_str: String = genres
            .iter()
            .filter_map(|g| g.get("name").and_then(|n| n.as_str()))
            .filter(|n| !n.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        if !genre_str.is_empty() {
            let existing = tag.get_string(&ItemKey::Genre).unwrap_or("");
            if crate::tags::should_write(existing, &genre_str, overwrite) {
                tag.insert_text(ItemKey::Genre, genre_str);
                changed = true;
            }
        }
    }
    // Mood: only set when empty (don't overwrite AudioMuse's mood).
    if let Some(moods) = data.get("moods").and_then(|m| m.as_array()) {
        let mood_str: String = moods
            .iter()
            .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
            .filter(|n| !n.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        if !mood_str.is_empty() {
            let existing = tag.get_string(&ItemKey::Unknown("MOOD".into())).unwrap_or("");
            if existing.is_empty() {
                tag.insert_text(ItemKey::Unknown("MOOD".into()), mood_str);
                changed = true;
            }
        }
    }
    if !changed {
        return false;
    }
    let _ = tagged.insert_tag(tag);
    crate::tags::save_tagged_atomic(&tagged, path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_rule() {
        // 3-min song, played 30s -> skipped at 30% threshold.
        assert!(is_skip(180, 30_000, 30));
        // Played 90s of 180 -> not a skip.
        assert!(!is_skip(180, 90_000, 30));
        assert!(!is_skip(0, 10, 30));
    }

    #[test]
    fn weight_and_rating() {
        assert_eq!(weight(10, 0), 10.0);
        assert_eq!(weight(10, 9), -8.0);
        assert!(rating_1_5(10, 0) >= 4);
        assert_eq!(rating_1_5(0, 0), 0);
        assert!(rating_1_5(2, 8) < rating_1_5(8, 2));
    }

    #[test]
    fn hard_exclude_only_when_net_negative() {
        // Skipped more than played, past cap, enough samples -> excluded.
        assert!(hard_exclude(1, 3, 0.6, 3));
        assert!(hard_exclude(2, 3, 0.6, 3));
        // Played as much as skipped: you keep coming back -> never excluded.
        assert!(!hard_exclude(3, 3, 0.6, 3));
        // You like it: more full plays than skips -> never excluded.
        assert!(!hard_exclude(5, 2, 0.6, 3));
        assert!(!hard_exclude(3, 1, 0.6, 3));
        // Not enough samples yet.
        assert!(!hard_exclude(0, 2, 0.6, 3));
        // A full play forgives a skip: 2 plays/3 skips (excluded) -> play in
        // full once more -> 3 plays/2 skips (kept).
        assert!(hard_exclude(2, 3, 0.6, 3));
        assert!(!hard_exclude(3, 2, 0.6, 3));
    }

    #[test]
    fn parses_nowplaying_and_playlist_id() {
        let np = r#"{"subsonic-response":{"nowPlaying":{"entry":[
            {"id":"s1","positionMs":10000,"duration":180,"title":"Alright","artist":"Electric Light Orchestra","album":"All Over the World"},
            {"id":"s2","positionMs":120000,"duration":240}
        ]}}}"#;
        let entries = parse_nowplaying(np);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].position_ms, 10_000);
        assert_eq!(entries[1].duration, 240);
        assert_eq!(entries[0].title, "Alright");
        assert_eq!(entries[0].artist, "Electric Light Orchestra");
        assert_eq!(entries[0].album, "All Over the World");
        assert_eq!(entries[1].title, "", "missing fields default to empty");

        assert_eq!(
            parse_playlist_id(r#"{"subsonic-response":{"playlist":{"id":"pl-9"}}}"#),
            Some("pl-9".into())
        );
    }

    #[test]
    fn describe_is_detailed_even_when_idle() {
        let r = StatsReport {
            plays: 1,
            skips: 2,
            events: vec![
                "full play: Electric Light Orchestra - Alright [All Over the World]".into(),
                "skipped: Radiohead - Creep [Pablo Honey] (stopped at 12% of 238s)".into(),
            ],
            total_plays: 40,
            total_skips: 9,
            tracked: 27,
            now_playing: vec![],
        };
        let msg = describe(&r, 12, 3, 2, 4);
        assert!(msg.contains("1 play(s), 2 skip(s) observed"), "{}", msg);
        assert!(msg.contains("Electric Light Orchestra - Alright"), "{}", msg);
        assert!(msg.contains("40 full plays, 9 skips across 27 tracked songs"), "{}", msg);
        assert!(msg.contains("Top Picks playlist refreshed with 12 tracks"), "{}", msg);
        assert!(msg.contains("3 skip-heavy track(s) removed"), "{}", msg);
        assert!(msg.contains("2 rating(s) published to Navidrome"), "{}", msg);
        // Idle cycle still reports useful cumulative state.
        let idle = StatsReport { plays: 0, skips: 0, events: vec![], total_plays: 40, total_skips: 9, tracked: 27, now_playing: vec![] };
        let idle_msg = describe(&idle, 0, 0, 0, 0);
        assert!(idle_msg.contains("no playback activity observed"), "{}", idle_msg);
        assert!(idle_msg.contains("40 full plays, 9 skips across 27 tracked songs"), "{}", idle_msg);
    }

    #[test]
    fn star_band_boundaries() {
        // 55 half / 85 full / 5 ignore.
        assert_eq!(star_band(50.0, 55, 85, 5), StarBand::Skip);
        assert_eq!(star_band(54.9, 55, 85, 5), StarBand::Skip);
        assert_eq!(star_band(55.0, 55, 85, 5), StarBand::Half);
        assert_eq!(star_band(84.0, 55, 85, 5), StarBand::Half);
        assert_eq!(star_band(85.0, 55, 85, 5), StarBand::Full);
        assert_eq!(star_band(100.0, 55, 85, 5), StarBand::Full);
        // Below the ignore threshold: ignored, no penalty.
        assert_eq!(star_band(0.0, 55, 85, 5), StarBand::Ignore);
        assert_eq!(star_band(4.9, 55, 85, 5), StarBand::Ignore);
        assert_eq!(star_band(5.0, 55, 85, 5), StarBand::Skip);
    }

    #[test]
    fn star_ignored_listens_do_not_penalize() {
        let t = StarTally { skips: 0, ..Default::default() };
        let after = apply_band(t, StarBand::Ignore);
        assert_eq!(after.skips, 0);
        assert_eq!(after.full, 0);
        assert_eq!(after.half, 0);
    }

    #[test]
    fn star_accumulates_with_penalty_and_cap() {
        // 3 full listens -> 3.0 stars (a single play can't max the rating).
        let t = apply_band(StarTally { full: 2, ..Default::default() }, StarBand::Full);
        assert_eq!(star_rating(&t), 3.0);
        // 5 full listens -> capped at 5.0 (credit never exceeds 5).
        let t = StarTally { full: 5, half: 2, skips: 1, ..Default::default() };
        assert_eq!(star_credit(&t), 5.0);
        assert_eq!(star_rating(&t), 5.0);
        // Below cap the credit is full + half*0.5 - skips*0.5.
        let t = StarTally { full: 3, half: 1, skips: 1, ..Default::default() };
        assert_eq!(star_credit(&t), 3.0);
        // 2 full + 2 half -> 3.0; skips penalize.
        let t = apply_band(apply_band(StarTally { full: 2, ..Default::default() }, StarBand::Half), StarBand::Half);
        assert_eq!(star_rating(&t), 3.0);
        // All skips -> 0.0 (penalty).
        let mut t = StarTally::default();
        for _ in 0..3 {
            t = apply_band(t, StarBand::Skip);
        }
        assert_eq!(star_rating(&t), 0.0);
    }

    #[test]
    fn star_full_listen_forgives_one_skip() {
        // 1 skip then 1 full: skip penalty is forgiven.
        let t = apply_band(StarTally::default(), StarBand::Skip);
        let t = apply_band(t, StarBand::Full);
        assert_eq!(t.skips, 0);
        assert_eq!(t.full, 1);
        assert_eq!(star_rating(&t), 1.0);
        // Half-step precision: 2 full + 1 skip -> 1.5 (not 1.0).
        let t = apply_band(StarTally { full: 2, ..Default::default() }, StarBand::Skip);
        assert_eq!(star_rating(&t), 1.5);
    }

    #[test]
    fn star_playcount_is_full_listens_only() {
        // Half listens and skips never increment the playcount.
        let t = apply_band(StarTally::default(), StarBand::Half);
        let t = apply_band(t, StarBand::Skip);
        assert_eq!(t.full, 0, "half/skip must not touch playcount");
        assert_eq!(star_credit(&t), 0.0);
    }
}

