// Libre.fm scrobble integration — free, open-source alternative to Last.fm.
// Uses the same Subsonic-compatible scrobble protocol. API key required from
// https://libre.fm/api-keys.php



#[cfg(target_arch = "wasm32")]
pub mod host_librefm {
    use crate::config::Config;
    use crate::net;
    use nd_pdk::host;
    use std::collections::HashMap;

    /// Scrobble a listen to Libre.fm using the Subsonic scrobble protocol.
    /// Libre.fm accepts the same POST format as Last.fm's track.scrobble.
    pub fn scrobble(
        cfg: &Config,
        artist: &str,
        title: &str,
        album: &str,
        ts: i64,
    ) {
        const BASE: &str = "https://libre.fm";
        let base = BASE;
        // Share Last.fm credentials when Libre.fm user not set
        let user = if cfg.librefm_user.trim().is_empty() { cfg.lastfm_user.trim() } else { cfg.librefm_user.trim() };
        let sk = if cfg.librefm_session_key.trim().is_empty() { String::new() } else { cfg.librefm_session_key.trim().to_string() };
        if user.is_empty() {
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
            crate::favorites::host_favorites::urlencode(user),
            ts,
            crate::favorites::host_favorites::urlencode(album),
            crate::favorites::host_favorites::urlencode(artist),
            crate::favorites::host_favorites::urlencode(&sk),
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
