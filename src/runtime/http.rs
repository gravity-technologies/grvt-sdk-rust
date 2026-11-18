//! HTTP replies as plain data (core), and the client's HTTP transport plug-in (feature `client`). The
//! default transport is one `reqwest` client (rustls, keep-alive pool).
//!
//! A custom transport (recording, replay, test double) implements `HttpTransport`. Errors must be
//! `Error::Transport` with `sent: false` only when the request provably did not leave the process
//! (e.g. connect refused); the runtime uses that to decide whether a write's outcome is unknown.

#[cfg(feature = "client")]
use std::time::Duration;

#[cfg(feature = "client")]
use futures::future::BoxFuture;

#[cfg(feature = "client")]
use super::error::{Error, Result};

#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Every value of a header (case-insensitive).
    pub fn header_values<'a, 'n: 'a>(&'a self, name: &'n str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers
            .iter()
            .filter(move |(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[cfg(feature = "client")]
/// Implementations must not replay POSTs or follow their redirects.
/// Honor the attempt timeout and cancellation by future drop.
/// Do not detach a queued write into background work that can start after the returned
/// future is dropped. A write whose send already started can remain uncertain.
pub trait HttpTransport: Send + Sync {
    fn post<'a>(
        &'a self,
        url: &'a str,
        body: Vec<u8>,
        headers: &'a [(String, String)],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<HttpResponse>>;

    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> BoxFuture<'a, Result<HttpResponse>>;
}

/// The default transport.
#[cfg(feature = "client")]
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

#[cfg(feature = "client")]
impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::with_builder(
            reqwest::Client::builder()
                .pool_max_idle_per_host(64)
                .pool_idle_timeout(Duration::from_secs(60))
                .tcp_nodelay(true),
        )
        .expect("reqwest client")
    }
}

#[cfg(feature = "client")]
impl ReqwestTransport {
    /// Customize connection settings while retaining the SDK's no-redirect, no-replay policy.
    pub fn with_builder(builder: reqwest::ClientBuilder) -> Result<Self> {
        let client = builder
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|e| Error::Config(format!("HTTP client: {e}")))?;
        Ok(Self { client })
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<HttpResponse> {
        let resp = req.send().await.map_err(|e| {
            // A connect failure means nothing left this process; anything later may have.
            Error::transport(format!("{e}"), None, !e.is_connect())
        })?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_owned(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::transport(format!("{e}"), None, true))?
            .to_vec();
        Ok(HttpResponse { status, headers, body })
    }
}

#[cfg(feature = "client")]
impl HttpTransport for ReqwestTransport {
    fn post<'a>(
        &'a self,
        url: &'a str,
        body: Vec<u8>,
        headers: &'a [(String, String)],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<HttpResponse>> {
        let mut req = self.client.post(url).body(body).timeout(timeout);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        Box::pin(self.send(req))
    }

    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> BoxFuture<'a, Result<HttpResponse>> {
        Box::pin(self.send(self.client.get(url).timeout(timeout)))
    }
}
