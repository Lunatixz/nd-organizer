// AudioMuse-AI integration: pulls acoustic analysis (BPM/key/mood/energy) from
// a self-hosted AudioMuse-AI instance (github.com/NeptuneHub/AudioMuse-AI,
// web UI on :8000) and writes it into track tags. The instance exposes its API
// schema at /apidocs/ (Swagger, JWT-gated). We call the analysis + refresh
// endpoints best-effort and fail-soft (skip + log) when unreachable.
//
// Circuit breaker: when AudioMuse-AI goes offline mid-run, acoustic work pauses
// (retry -> cooldown, same windows as AcoustID), a throttled probe recovers it,
// and after the long timeout we degrade (proceed; fetch fail-fasts) so the
// organizer never blocks on it.

use std::collections::HashMap;
use std::path::Path;

use lofty::prelude::*;
use nd_pdk::host;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const CIRCUIT_KEY: &str = "audiomuse";

fn circuit_clear() {
    crate::net::circuit_clear(CIRCUIT_KEY);
}

fn circuit_mark_failed() {
    crate::net::circuit_mark_failed(CIRCUIT_KEY);
}

/// True while the circuit is open (any stage) - callers fail fast instead of
/// POSTing to a dead service.
pub fn circuit_open() -> bool {
    crate::net::circuit_open(CIRCUIT_KEY)
}

/// True when we should pause acoustic work while AudioMuse-AI recovers. During
/// the retry/cooldown window a throttled probe checks for recovery; once the
/// long timeout (degraded) elapses we proceed (fetch then fail-fasts).
pub fn should_pause(cfg: &crate::config::Config) -> bool {
    if !crate::net::circuit_paused(CIRCUIT_KEY) {
        return false;
    }
    if !crate::net::throttle("audiomuse.circuit", 60_000) {
        return true;
    }
    if probe_up(cfg) {
        circuit_clear();
        return false;
    }
    true
}

fn probe_up(cfg: &crate::config::Config) -> bool {
    let base = resolve_base(cfg);
    if base.is_empty() {
        return false;
    }
    let req = host::http::HTTPRequest {
        method: "GET".into(),
        url: base.to_string(),
        headers: headers(cfg),
        no_follow_redirects: false,
        body: vec![],
        timeout_ms: 20_000,
    };
    matches!(host::http::send(req), Ok(Some(resp)) if resp.status_code < 400)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Acoustic {
    pub bpm: Option<f64>,
    pub key: Option<String>,
    pub mood: Option<String>,
    pub energy: Option<f64>,
}

#[derive(Deserialize)]
struct RawAcoustic {
    #[serde(default)]
    bpm: Option<f64>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    mood: Option<String>,
    #[serde(default)]
    energy: Option<f64>,
}

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn headers(cfg: &crate::config::Config) -> HashMap<String, String> {
    let mut h = HashMap::new();
    if !cfg.audiomuse_token.trim().is_empty() {
        h.insert(
            "Authorization".to_string(),
            format!("Bearer {}", cfg.audiomuse_token.trim()),
        );
    }
    h
}

/// Resolve the AudioMuse-AI base URL to use. Tries the configured URL first; if
/// it is unreachable from inside the Navidrome container (its docker network may
/// not be routable from Navidrome's), falls back to the server's own host address
/// on the same port - the host-published port always answers. Cached 10 min.
pub fn resolve_base(cfg: &crate::config::Config) -> String {
    crate::net::cached("audiomuse.base", 600, || {
        let configured = cfg.audiomuse_url.trim().trim_end_matches('/').to_string();
        if configured.is_empty() {
            return Some(configured);
        }
        let mut candidates: Vec<String> = vec![configured.clone()];
        // Same host as Navidrome itself (its BaseUrl/address), same port.
        if let Some(host) = crate::wasm::server_host() {
            let port = configured
                .rsplit(':')
                .next()
                .map(|p| p.split('/').next().unwrap_or(p))
                .filter(|p| p.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or("8000")
                .to_string();
            candidates.push(format!("http://{host}:{port}"));
        }
        // Docker's host alias + common bridge gateway reach host-published ports.
        for host in ["host.docker.internal", "172.17.0.1"] {
            if let Some(alt) = crate::wasm::host_replace(&configured, host) {
                if !candidates.contains(&alt) {
                    candidates.push(alt);
                }
            }
        }
        // The subnet gateway (x.x.x.1) of the configured address can reach the
        // host-published port too, no Docker changes required.
        if let Some(gw) = crate::wasm::subnet_gateway(&configured) {
            if !candidates.contains(&gw) {
                candidates.push(gw);
            }
        }
        // Probe candidates with a short timeout (discovery only - a live
        // AudioMuse answers its root fast). Per-URL results are cached 1h.
        for c in candidates {
            if crate::wasm::probe_ok_timeout("audiomuse", &c, &headers(cfg), 10_000, 3600).is_none() {
                return Some(c);
            }
        }
        Some(configured) // nothing reachable - report the configured URL's failure
    })
    .unwrap_or_default()
}

/// Ask AudioMuse-AI to re-run library analysis after file renames
/// (notifyAudiomuseAfterRun). This is AudioMuse-AI's real re-sync trigger:
/// `POST /api/analysis/start` enqueues an analysis task for recent albums.
pub fn re_sync(cfg: &crate::config::Config) -> Result<(), String> {
    let base = resolve_base(cfg);
    if base.is_empty() {
        return Ok(());
    }
    let req = host::http::HTTPRequest {
        method: "POST".into(),
        url: format!("{base}/api/analysis/start"),
        headers: headers(cfg),
        no_follow_redirects: false,
        body: b"{}".to_vec(),
        timeout_ms: 15_000,
    };
    match host::http::send(req) {
        Ok(Some(resp)) if (200..300).contains(&resp.status_code) => {
            circuit_clear();
            Ok(())
        }
        Ok(Some(resp)) if resp.status_code == 409 => {
            circuit_clear(); // analysis already running - reachable
            Ok(())
        }
        Ok(Some(resp)) => {
            circuit_mark_failed();
            Err(format!(
                "AudioMuse-AI analysis start HTTP {}",
                resp.status_code
            ))
        }
        Ok(None) => {
            circuit_mark_failed();
            Err("AudioMuse-AI analysis start: no response".into())
        }
        Err(e) => {
            circuit_mark_failed();
            Err(format!("AudioMuse-AI analysis start failed: {e}"))
        }
    }
}

/// Fetch per-track acoustic analysis. NOTE: AudioMuse-AI exposes per-track
/// BPM/key/mood/energy only through its in-process plugin API
/// (`get_score_data_by_ids`); there is no public HTTP endpoint for them in the
/// core app. This attempts the analysis route and fails soft (None + log), so
/// `writeAcousticTags` never blocks a run. If your AudioMuse-AI instance (or an
/// installed acoustic-tags plugin) exposes a per-track HTTP endpoint, point it
/// here.
pub fn fetch(cfg: &crate::config::Config, artist: &str, title: &str) -> Option<Acoustic> {
    let base = resolve_base(cfg);
    if base.is_empty() || artist.trim().is_empty() || title.trim().is_empty() {
        return None;
    }
    if !crate::net::circuit_probe(CIRCUIT_KEY, &base, &headers(cfg), 20_000) {
        return None; // offline - fail fast (auto-recovers via probe)
    }
    let cache_key = format!("am:{}|{}", artist.to_lowercase(), title.to_lowercase());
    if let Ok(Some(v)) = crate::store::kv().get(&cache_key) {
        if let Ok(a) = serde_json::from_slice::<Acoustic>(&v) {
            return Some(a);
        }
    }
    if !crate::net::throttle("audiomuse", 1000) {
        return None;
    }
    let url = format!(
        "{base}/api/analysis?artist={}&track={}",
        urlenc(artist),
        urlenc(title)
    );
    let req = host::http::HTTPRequest {
        method: "GET".into(),
        url,
        headers: headers(cfg),
        no_follow_redirects: false,
        body: vec![],
        timeout_ms: 15_000,
    };
    let ac: Option<Acoustic> = match host::http::send(req) {
        Ok(Some(resp)) if resp.status_code == 200 => {
            circuit_clear();
            serde_json::from_slice::<Value>(&resp.body).ok().and_then(|v| {
                serde_json::from_value::<RawAcoustic>(v).ok().map(|r| Acoustic {
                    bpm: r.bpm,
                    key: r.key.filter(|k| !k.is_empty()),
                    mood: r.mood.filter(|m| !m.is_empty()),
                    energy: r.energy,
                })
            })
        }
        Ok(Some(resp)) if resp.status_code == 405 || resp.status_code == 404 => {
            // AudioMuse-AI has no per-track features endpoint in the core app;
            // log once so the user knows acoustic-tag writes are skipped. This
            // is a permanent API gap, not an outage - don't trip the circuit.
            crate::wasm::log_info(
                "audiomuse: per-track acoustic features (BPM/key/mood) are not exposed \
                 over HTTP by AudioMuse-AI - writeAcousticTags will be skipped unless an \
                 acoustic-tags plugin exposes them",
            );
            None
        }
        Ok(Some(_)) => {
            circuit_mark_failed();
            None
        }
        Ok(None) | Err(_) => {
            circuit_mark_failed();
            None
        }
    };
    if let Some(a) = &ac {
        let _ = crate::store::kv().set_with_ttl(
            &cache_key,
            serde_json::to_vec(a).unwrap_or_default(),
            7 * 24 * 3600,
        );
    }
    ac
}

/// Write acoustic tags (BPM / KEY / MOOD / ENERGY) into a track.
pub fn write_tags(path: &Path, ac: &Acoustic, overwrite: bool) -> Result<(), String> {
    let mut tagged = lofty::read_from_path(path).map_err(|e| e.to_string())?;
    let mut tag = tagged.primary_tag().ok_or("no tag block")?.to_owned();
    let mut changed = false;
    if let Some(bpm) = ac.bpm {
        let val = format!("{bpm:.0}");
        let existing = tag.get_string(&ItemKey::Bpm).unwrap_or("");
        if crate::tags::should_write(existing, &val, overwrite) {
            tag.insert_text(ItemKey::Bpm, val);
            changed = true;
        }
    }
    if let Some(key) = &ac.key {
        let existing = tag.get_string(&ItemKey::InitialKey).unwrap_or("");
        if crate::tags::should_write(existing, key, overwrite) {
            tag.insert_text(ItemKey::InitialKey, key.clone());
            changed = true;
        }
    }
    if let Some(mood) = &ac.mood {
        let existing = tag.get_string(&ItemKey::Unknown("MOOD".into())).unwrap_or("");
        if crate::tags::should_write(existing, mood, overwrite) {
            tag.insert_text(ItemKey::Unknown("MOOD".into()), mood.clone());
            changed = true;
        }
    }
    if let Some(energy) = ac.energy {
        let val = format!("{energy:.2}");
        let existing = tag.get_string(&ItemKey::Unknown("ENERGY".into())).unwrap_or("");
        if crate::tags::should_write(existing, &val, overwrite) {
            tag.insert_text(ItemKey::Unknown("ENERGY".into()), val);
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let _ = tagged.insert_tag(tag);
    crate::tags::save_tagged_atomic(&tagged, path)
}
