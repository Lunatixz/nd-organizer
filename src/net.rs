// Outbound HTTP helpers: KVStore-backed TTL caching + per-host throttling so
// external APIs (Last.fm, MusicBrainz, AcoustID, Lidarr, ...) are never
// hammered. Two complementary levers:
//
//   - `cached(scope, ttl, compute)`: a fresh TTL cache means `compute` is NOT
//     called again until it expires - this is what throttles repeated work
//     (health checks, auth, lookups) and serves the stored DB value instead.
//   - `throttle(host, min_interval_ms)`: hard per-host minimum interval so even
//     cache misses (or uncached calls) stay under a rate ceiling.
//
// All state lives in the plugin KVStore, so it survives restarts.

use serde::de::DeserializeOwned;
use serde::Serialize;

use std::collections::HashMap;

#[cfg(target_arch = "wasm32")]
use nd_pdk::host;

/// Unix seconds.
pub fn now_secs() -> i64 {
    crate::state::now_ts()
}

// ------------------------------------------------------------------ circuits
//
// Generic circuit breaker for any external provider (AcoustID, MusicBrainz,
// Lidarr, Last.fm, LRCLIB, Cover Art Archive, AudioMuse-AI, ...). State is just
// a first-failure timestamp in KVStore; the stage is derived from age (retry ->
// cooldown -> degraded, same windows as AcoustID). Callers:
//   - fail fast (`circuit_open`) instead of POSTing/GETting a dead service,
//   - pause their work during retry/cooldown (`circuit_paused`),
//   - proceed after the long timeout (degraded) - the fetch then fail-fasts,
//   - clear on success, mark on transport failure.

pub fn circuit_since(provider: &str) -> Option<i64> {
    crate::store::kv()
        .get(&format!("circuit.{provider}"))
        .ok()
        .flatten()
        .and_then(|v| String::from_utf8_lossy(&v).parse().ok())
}

pub fn circuit_open(provider: &str) -> bool {
    circuit_since(provider).is_some()
}

pub fn circuit_clear(provider: &str) {
    let _ = crate::store::kv().delete(&format!("circuit.{provider}"));
}

/// Record a failure. Keeps the FIRST failure time so repeated failures don't
/// keep pushing the retry/cooldown windows out.
pub fn circuit_mark_failed(provider: &str) {
    if circuit_since(provider).is_none() {
        let _ = crate::store::kv()
            .set(&format!("circuit.{provider}"), crate::state::now_ts().to_string().into_bytes());
    }
}

pub fn circuit_stage(provider: &str) -> Option<crate::state::AcoustidStage> {
    Some(crate::state::acoustid_stage(
        circuit_since(provider)?,
        crate::state::now_ts(),
    ))
}

/// True during the retry/cooldown window - the dependent work should pause and
/// wait for recovery.
pub fn circuit_paused(provider: &str) -> bool {
    matches!(
        circuit_stage(provider),
        Some(crate::state::AcoustidStage::Retry | crate::state::AcoustidStage::Cooldown)
    )
}

/// Fail-fast gate with auto-recovery. When the provider's circuit is open we
/// run a throttled live probe (once per minute); if it answers, the circuit
/// clears and we proceed. Returns true when the provider is reachable NOW.
/// This is what lets a provider resume ~60s after coming back online without
/// hammering it while it's down.
pub fn circuit_probe(
    provider: &str,
    url: &str,
    headers: &HashMap<String, String>,
    timeout_ms: i32,
) -> bool {
    if !circuit_open(provider) {
        return true;
    }
    if !throttle(&format!("circuit.probe.{provider}"), 60_000) {
        return false;
    }
    let req = host::http::HTTPRequest {
        method: "GET".into(),
        url: url.to_string(),
        headers: headers.clone(),
        no_follow_redirects: false,
        body: vec![],
        timeout_ms,
    };
    let up = matches!(host::http::send(req), Ok(Some(resp)) if resp.status_code < 400);
    if up {
        circuit_clear(provider);
    }
    up
}

/// Live reachability check that ALSO records the outcome on the circuit (same
/// semantics as the fetchers: transport failure / no response / 5xx = down, a
/// 4xx means the service is up but misconfigured). Used by the run gate so a
/// freshly-unreachable provider is caught on the very first run - not just after
/// a lookup happens to fail - without relying on cached probe results.
pub fn circuit_check(
    provider: &str,
    url: &str,
    headers: &HashMap<String, String>,
    timeout_ms: i32,
) -> bool {
    let req = host::http::HTTPRequest {
        method: "GET".into(),
        url: url.to_string(),
        headers: headers.clone(),
        no_follow_redirects: false,
        body: vec![],
        timeout_ms,
    };
    match host::http::send(req) {
        Ok(Some(resp)) if resp.status_code < 400 => {
            circuit_clear(provider);
            true
        }
        Ok(Some(resp)) if resp.status_code >= 500 => {
            circuit_mark_failed(provider);
            false
        }
        Ok(Some(_)) => true, // 4xx = reachable (e.g. bad credentials) - not an outage
        Ok(None) => {
            circuit_mark_failed(provider);
            false
        }
        Err(_) => {
            circuit_mark_failed(provider);
            false
        }
    }
}

/// TTL cache in the KVStore. On a hit it returns the stored value without
/// calling `compute`; on a miss it runs `compute`, stores the result for
/// `ttl_secs`, and returns it. `None` results are NOT cached (so a transient
/// failure is retried on the next pass).
#[cfg(target_arch = "wasm32")]
pub fn cached<T>(scope: &str, ttl_secs: u64, compute: impl FnOnce() -> Option<T>) -> Option<T>
where
    T: Serialize + DeserializeOwned,
{
    let now = now_secs();
    let ttl_key = format!("net.ttl.{scope}");
    if let Ok(Some(v)) = crate::store::kv().get(&ttl_key) {
        if let Ok(exp) = String::from_utf8_lossy(&v).parse::<i64>() {
            if exp > now {
                if let Ok(Some(cv)) = crate::store::kv().get(&format!("net.cache.{scope}")) {
                    if let Ok(t) = serde_json::from_slice::<T>(&cv) {
                        return Some(t);
                    }
                }
            }
        }
    }
    let value = compute()?;
    let _ = crate::store::kv().set(&ttl_key, (now + ttl_secs as i64).to_string().into_bytes());
    if let Ok(bytes) = serde_json::to_vec(&value) {
        let _ = crate::store::kv().set(&format!("net.cache.{scope}"), bytes);
    }
    Some(value)
}

/// Hard per-host throttle: returns true when a call may proceed, false when one
/// was made within `min_interval_ms`. On success it records the call time.
#[cfg(target_arch = "wasm32")]
pub fn throttle(host: &str, min_interval_ms: u64) -> bool {
    let key = format!("net.throttle.{host}");
    let now_ms = now_secs() * 1000;
    if let Ok(Some(v)) = crate::store::kv().get(&key) {
        if let Ok(last) = String::from_utf8_lossy(&v).parse::<i64>() {
            if now_ms - last < min_interval_ms as i64 {
                return false;
            }
        }
    }
    let _ = crate::store::kv().set(&key, now_ms.to_string().into_bytes());
    true
}

