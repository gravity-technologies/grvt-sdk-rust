//! The client's API-key session: logs in with the core session state machine (`session.rs`), shares one
//! login between concurrent callers, and keeps the state the sockets and REST calls read.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::env::Environment;
use super::error::{Error, Result};
use super::http::HttpTransport;
pub use super::session::{
    parse_http_date, SessionState, ACCOUNT_HEADER, COOKIE_NAME, DEFAULT_TTL, LOGIN_PATH, REFRESH_AT,
};

pub struct Session {
    api_key: Option<String>,
    env: Environment,
    http: Arc<dyn HttpTransport>,
    http_attempt_timeout: Duration,
    state: Mutex<SessionState>,
    login_lock: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("enabled", &self.enabled())
            .field("generation", &self.generation())
            .finish()
    }
}

pub(crate) fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl Session {
    pub fn new(
        api_key: Option<String>,
        env: Environment,
        http: Arc<dyn HttpTransport>,
        http_attempt_timeout: Duration,
    ) -> Session {
        Session {
            api_key,
            env,
            http,
            http_attempt_timeout,
            state: Mutex::default(),
            login_lock: tokio::sync::Mutex::new(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn http_attempt_timeout(&self) -> Duration {
        self.http_attempt_timeout
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn enabled(&self) -> bool {
        self.api_key.is_some()
    }

    pub fn snapshot(&self) -> SessionState {
        self.lock().clone()
    }

    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Cookie and account headers for an authenticated call (empty before login).
    pub fn headers(&self) -> Vec<(String, String)> {
        self.lock().headers()
    }

    /// Run `f` on the current state without copying it.
    pub fn with_state<T>(&self, f: impl FnOnce(&SessionState) -> T) -> T {
        f(&self.lock())
    }

    /// Log in if there is no fresh session. Concurrent callers share one login.
    pub async fn ensure(&self) -> Result<()> {
        if !self.enabled() || self.lock().fresh(now()) {
            return Ok(());
        }
        let _g = self.login_lock.lock().await;
        if !self.lock().fresh(now()) {
            self.login().await?;
        }
        Ok(())
    }

    pub async fn login(&self) -> Result<()> {
        let key = self
            .api_key
            .as_deref()
            .ok_or_else(|| Error::Config("this call needs a session: set Config::api_key".into()))?;
        let req = super::session::api_key_login_request(&self.env, key);
        let resp = self
            .http
            .post(&req.url, req.body, &req.headers, self.http_attempt_timeout)
            .await?;
        let mut s = self.lock();
        let t = now();
        s.on_login_response(resp.status, &resp.headers, &resp.body, t)?;
        tracing::debug!(target: "grvt", "logged in, session generation {} valid for {:.0}s", s.generation, s.expires_at - t);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::http::HttpResponse;
    use super::*;
    use futures::future::BoxFuture;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct LoginHttp {
        calls: AtomicUsize,
        started: tokio::sync::Notify,
    }
    impl HttpTransport for LoginHttp {
        fn post<'a>(
            &'a self,
            _: &'a str,
            _: Vec<u8>,
            _: &'a [(String, String)],
            _: Duration,
        ) -> BoxFuture<'a, Result<HttpResponse>> {
            Box::pin(async move {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.started.notify_one();
                    std::future::pending::<()>().await;
                }
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![("Set-Cookie".into(), "gravity=test; Max-Age=3600".into())],
                    body: b"{}".to_vec(),
                })
            })
        }
        fn get<'a>(&'a self, _: &'a str, _: Duration) -> BoxFuture<'a, Result<HttpResponse>> {
            Box::pin(std::future::pending())
        }
    }
    #[tokio::test]
    async fn cancelled_login_initializer_does_not_fail_other_waiter() {
        let http = Arc::new(LoginHttp {
            calls: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
        });
        let session = Arc::new(Session::new(
            Some("key".into()),
            Environment::testnet(),
            http.clone(),
            Duration::from_secs(10),
        ));
        let first = {
            let session = session.clone();
            tokio::spawn(async move { session.ensure().await })
        };
        http.started.notified().await;
        let second = {
            let session = session.clone();
            tokio::spawn(async move { session.ensure().await })
        };
        tokio::task::yield_now().await;
        first.abort();
        tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(http.calls.load(Ordering::SeqCst), 2);
        assert_eq!(session.generation(), 1);
    }
}
