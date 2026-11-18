//! API-key session state: what to send and when it expires. No I/O: the client (or the application) does
//! the HTTP.
//!
//! Login: POST {edge}/auth/api_key/login {"api_key": ...}
//!   -> Set-Cookie: gravity=...; Expires=...
//!   -> X-Grvt-Account-Id header, which gateways check as a sticky-session token
//!
//! Both go on every authenticated REST call and on the WS upgrade request. A WS connection keeps the
//! session it was opened with (there is no in-band re-auth), so log in again before `refresh_due()` and
//! move sockets to new connections opened with the new headers.

use std::time::Duration;

use serde::Deserialize;

use super::env::Environment;
use super::error::{Error, Result};
use super::request::HttpRequest;
use crate::generated::behavior;

pub const COOKIE_NAME: &str = "gravity";
pub const ACCOUNT_HEADER: &str = "X-Grvt-Account-Id";
pub const LOGIN_PATH: &str = "/auth/api_key/login";
/// Used when the cookie carries no expiry.
pub const DEFAULT_TTL: Duration = behavior::SESSION_DEFAULT_LIFETIME;
/// Re-login at this share of the cookie lifetime.
pub const REFRESH_AT: f64 = behavior::SESSION_RENEW_AT;
pub(crate) const LOGIN_RPC: &str = "auth/api_key/login";

/// The api-key login request.
pub fn api_key_login_request(env: &Environment, api_key: &str) -> HttpRequest {
    HttpRequest {
        method: "POST",
        url: format!("{}{LOGIN_PATH}", env.edge_url),
        headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
        body: serde_json::to_vec(&serde_json::json!({ "api_key": api_key })).expect("json"),
    }
}

/// The current login. Empty until the first successful `on_login_response`. Times are Unix seconds.
/// `{:?}` shows the cookie as `<redacted>`; read the `cookie` field when you need it.
#[derive(Clone, Default)]
pub struct SessionState {
    pub cookie: Option<String>,
    pub account_id: Option<String>,
    pub issued_at: f64,
    pub expires_at: f64,
    /// Bumps on every successful login: sockets opened with an older generation use the old session.
    pub generation: u64,
    pub funding_account_address: Option<String>,
}

impl std::fmt::Debug for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionState")
            .field("cookie", &self.cookie.as_ref().map(|_| "<redacted>"))
            .field("account_id", &self.account_id)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .field("generation", &self.generation)
            .field("funding_account_address", &self.funding_account_address)
            .finish()
    }
}

impl SessionState {
    pub fn logged_in(&self) -> bool {
        self.cookie.is_some()
    }

    /// Wall-clock seconds at which a re-login should happen.
    pub fn refresh_due(&self) -> f64 {
        self.issued_at + (self.expires_at - self.issued_at) * REFRESH_AT
    }

    /// Logged in and not yet due for a refresh.
    pub fn fresh(&self, now: f64) -> bool {
        self.logged_in() && now < self.refresh_due()
    }

    /// Cookie and account headers for an authenticated call or WS upgrade (empty before login).
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut h = Vec::new();
        if let Some(c) = &self.cookie {
            h.push(("Cookie".to_owned(), format!("{COOKIE_NAME}={c}")));
            if let Some(a) = &self.account_id {
                h.push((ACCOUNT_HEADER.to_owned(), a.clone()));
            }
        }
        h
    }

    /// Read a login reply. 401/403 are permanent until the key is fixed (`Api`, not retryable); 5xx is a
    /// retryable `Api` error. On success the session moves to the new cookie and `generation` bumps.
    pub fn on_login_response(
        &mut self,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
        now: f64,
    ) -> Result<()> {
        if status != 200 {
            #[derive(Deserialize)]
            struct E {
                error: Option<String>,
            }
            let msg = serde_json::from_slice::<E>(body)
                .ok()
                .and_then(|e| e.error)
                .unwrap_or_else(|| "login failed".into());
            return Err(Error::Api {
                code: 1000,
                message: msg,
                status: Some(status),
                rpc: Some(LOGIN_RPC.into()),
                retryable: status >= 500,
            });
        }
        let (cookie, expires) = parse_session_cookie(headers, now);
        let cookie = cookie.ok_or_else(|| Error::Api {
            code: 1000,
            message: "login response had no gravity cookie".into(),
            status: Some(status),
            rpc: Some(LOGIN_RPC.into()),
            retryable: false,
        })?;
        #[derive(Deserialize)]
        struct Body {
            funding_account_address: Option<String>,
        }
        self.funding_account_address = serde_json::from_slice::<Body>(body)
            .ok()
            .and_then(|b| b.funding_account_address);
        self.cookie = Some(cookie);
        self.account_id = header(headers, ACCOUNT_HEADER).map(str::to_owned);
        self.issued_at = now;
        self.expires_at = match expires {
            Some(e) if e > now => e,
            _ => now + DEFAULT_TTL.as_secs_f64(),
        };
        self.generation += 1;
        Ok(())
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The `gravity` cookie value and its expiry (Unix seconds) from Set-Cookie headers (Max-Age wins over
/// Expires).
pub fn parse_session_cookie(headers: &[(String, String)], now: f64) -> (Option<String>, Option<f64>) {
    for (_, line) in headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie")) {
        let mut parts = line.split(';').map(str::trim);
        let Some(first) = parts.next() else { continue };
        let Some(value) = first.strip_prefix(COOKIE_NAME).and_then(|v| v.strip_prefix('=')) else {
            continue;
        };
        let mut exp = None;
        for attr in parts {
            let (k, v) = attr.split_once('=').unwrap_or((attr, ""));
            if k.eq_ignore_ascii_case("max-age") {
                if let Ok(secs) = v.trim().parse::<f64>() {
                    exp = Some(now + secs);
                    break;
                }
            } else if k.eq_ignore_ascii_case("expires") && exp.is_none() {
                exp = parse_http_date(v.trim());
            }
        }
        return (Some(value.to_owned()), exp);
    }
    (None, None)
}

/// RFC 1123 date ("Wed, 21 Oct 2015 07:28:00 GMT") -> Unix seconds.
pub fn parse_http_date(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ]
    .iter()
    .position(|m| parts[2].to_ascii_lowercase().starts_with(m))? as i64
        + 1;
    let year: i64 = parts[3].parse().ok()?;
    let hms: Vec<i64> = parts[4].split(':').filter_map(|x| x.parse().ok()).collect();
    if hms.len() != 3 {
        return None;
    }
    // days from civil (Howard Hinnant)
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + hms[0] * 3600 + hms[1] * 60 + hms[2]) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(k: &str, v: &str) -> (String, String) {
        (k.to_owned(), v.to_owned())
    }

    #[test]
    fn cookie_and_date() {
        assert_eq!(parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"), Some(1445412480.0));
        let headers = [h(
            "Set-Cookie",
            "gravity=abc; Path=/; Expires=Wed, 21 Oct 2015 07:28:00 GMT",
        )];
        assert_eq!(
            parse_session_cookie(&headers, 0.0),
            (Some("abc".into()), Some(1445412480.0))
        );
        let max_age = [h(
            "set-cookie",
            "gravity=abc; Expires=Wed, 21 Oct 2015 07:28:00 GMT; Max-Age=60",
        )];
        assert_eq!(parse_session_cookie(&max_age, 100.0), (Some("abc".into()), Some(160.0)));
    }

    #[test]
    fn login_reply_moves_the_session() {
        let env = Environment::testnet();
        let req = api_key_login_request(&env, "k-1");
        assert_eq!(req.url, "https://edge.testnet.grvt.io/auth/api_key/login");
        assert_eq!(req.body, br#"{"api_key":"k-1"}"#);

        let mut s = SessionState::default();
        let headers = [
            h("Set-Cookie", "gravity=c-1; Path=/; Max-Age=1000"),
            h("x-grvt-account-id", "ACC:1"),
        ];
        s.on_login_response(200, &headers, br#"{"funding_account_address":"0xabc"}"#, 1000.0)
            .unwrap();
        assert_eq!(
            (s.cookie.as_deref(), s.account_id.as_deref()),
            (Some("c-1"), Some("ACC:1"))
        );
        assert_eq!((s.issued_at, s.expires_at, s.generation), (1000.0, 2000.0, 1));
        assert_eq!(s.refresh_due(), 1800.0);
        assert!(s.fresh(1799.0) && !s.fresh(1800.0));
        assert_eq!(
            s.headers(),
            vec![h("Cookie", "gravity=c-1"), h("X-Grvt-Account-Id", "ACC:1")]
        );

        let denied = s.on_login_response(401, &[], br#"{"error":"bad key"}"#, 1500.0);
        assert!(
            matches!(denied, Err(Error::Api { retryable: false, status: Some(401), ref message, .. }) if message == "bad key")
        );
        assert!(matches!(
            s.on_login_response(503, &[], b"", 1500.0),
            Err(Error::Api { retryable: true, .. })
        ));
        assert!(s.on_login_response(200, &[], b"{}", 1500.0).is_err());
        assert_eq!(s.generation, 1, "a failed login keeps the session");
    }

    #[test]
    fn debug_never_prints_the_cookie() {
        let s = SessionState {
            cookie: Some("cookie-secret-123".into()),
            account_id: Some("acc-1".into()),
            ..Default::default()
        };
        for out in [format!("{s:?}"), format!("{s:#?}")] {
            assert!(!out.contains("cookie-secret-123"), "{out}");
            assert!(out.contains("<redacted>") && out.contains("acc-1"), "{out}");
        }
    }
}
