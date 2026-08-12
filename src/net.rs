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

#[cfg(target_arch = "wasm32")]
use nd_pdk::host;

/// Unix seconds.
pub fn now_secs() -> i64 {
    crate::state::now_ts()
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
    if let Ok(Some(v)) = host::kvstore::get(&ttl_key) {
        if let Ok(exp) = String::from_utf8_lossy(&v).parse::<i64>() {
            if exp > now {
                if let Ok(Some(cv)) = host::kvstore::get(&format!("net.cache.{scope}")) {
                    if let Ok(t) = serde_json::from_slice::<T>(&cv) {
                        return Some(t);
                    }
                }
            }
        }
    }
    let value = compute()?;
    let _ = host::kvstore::set(&ttl_key, (now + ttl_secs as i64).to_string().into_bytes());
    if let Ok(bytes) = serde_json::to_vec(&value) {
        let _ = host::kvstore::set(&format!("net.cache.{scope}"), bytes);
    }
    Some(value)
}

/// Hard per-host throttle: returns true when a call may proceed, false when one
/// was made within `min_interval_ms`. On success it records the call time.
#[cfg(target_arch = "wasm32")]
pub fn throttle(host: &str, min_interval_ms: u64) -> bool {
    let key = format!("net.throttle.{host}");
    let now_ms = now_secs() * 1000;
    if let Ok(Some(v)) = host::kvstore::get(&key) {
        if let Ok(last) = String::from_utf8_lossy(&v).parse::<i64>() {
            if now_ms - last < min_interval_ms as i64 {
                return false;
            }
        }
    }
    let _ = host::kvstore::set(&key, now_ms.to_string().into_bytes());
    true
}
