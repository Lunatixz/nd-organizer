// Libre.fm scrobble integration — free, open-source alternative to Last.fm.
// Uses the same Subsonic-compatible scrobble protocol. API key required from
// https://libre.fm/api-keys.php

use std::collections::HashMap;

#[cfg(target_arch = "wasm32")]
pub mod host_librefm {
    use crate::config::Config;
    use crate::net;
    use nd_pdk::host;

    use super::*;

    /// Scrobble a listen to Libre.fm using the Subsonic scrobble protocol.
    /// Libre.fm accepts the same POST format as Last.fm's track.scrobble.
    pub fn scrobble(
        cfg: &Config,
        artist: &str,
        title: &str,
        album: &str,
        ts: i64,
    ) {
        let base = cfg.librefm_url.trim().trim_end_matches('/');
        if base.is_empty() || cfg.librefm_user.is_empty() {
            return;
        }
        if !net::circuit_probe(
            "librefm",
            base,
            &HashMap::new(),
            10_000,
        ) {
            crate::wasm::log_warn("Libre.fm offline (circuit open)");
            return;
        }
        // Libre.fm uses the Subsonic API format for scrobbling
        let params = format!(
            "s={}&u={}&v=1.13.0&c=nd-organizer&t={}&a={}&ar={}&sk={}&fmt=json",
            crate::favorites::host_favorites::urlencode(title),
            crate::favorites::host_favorites::urlencode(&cfg.librefm_user),
            ts,
            crate::favorites::host_favorites::urlencode(album),
            crate::favorites::host_favorites::urlencode(artist),
            crate::favorites::host_favorites::urlencode(&cfg.librefm_session_key),
        );
        let url = format!("{base}/rest/scrobble?{params}");
        let req = host::http::HTTPRequest {
            method: "GET".into(),
            url,
            headers: HashMap::new(),
            no_follow_redirects: false,
            body: vec![],
            timeout_ms: 10_000,
        };
        match host::http::send(req) {
            Ok(Some(resp)) if resp.status_code == 200 => {
                net::circuit_clear("librefm");
                crate::wasm::log_info("Libre.fm scrobble OK");
            }
            _ => {
                net::circuit_mark_failed("librefm");
            }
        }
    }
}
