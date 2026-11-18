//! The client's REST calls: POST the request the core built, retry reads, classify replies with the core.
//!
//! Success: HTTP 200, the body is the RPC's response struct.
//! Failure: body `{"code": int, "message": str, "status": int}`.
//! Reads retry with full jitter on retryable failures; writes are sent exactly once.

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use super::error::{Error, Result};
use super::hooks::{Event, Hooks};
use super::http::HttpTransport;
use super::lite::Encoding;
use super::log::{debug_enabled, redact, redact_header};
use super::protocol::Backoff;
use super::request::{may_retry, write_outcome, HttpRequest};
use super::spec::RpcSpec;
use crate::generated::behavior;

pub use super::request::READ_ATTEMPTS;

pub struct RestCall<'a> {
    pub http: &'a dyn HttpTransport,
    /// From `request::rest_request_body`.
    pub request: HttpRequest,
    pub spec: &'static RpcSpec,
    pub timeout: Duration,
    pub deadline: Instant,
    pub dispatch: &'a AtomicU8,
    pub hooks: &'a Hooks,
    pub client_order_ids: Vec<u64>,
    /// The client's encoding: an error body is read in it (edge routes are always full).
    pub encoding: Encoding,
}

/// The backoff between read retries.
pub(crate) fn read_backoff() -> Backoff {
    Backoff::new(behavior::READ_BACKOFF_BASE, behavior::READ_BACKOFF_CAP)
}

/// Returns the 200 body.
pub async fn rest_call(c: RestCall<'_>) -> Result<Vec<u8>> {
    let mut backoff = read_backoff();
    let mut attempt = 0;
    loop {
        attempt += 1;
        let err = match once(&c).await {
            Ok(body) => return Ok(body),
            Err(e) if c.spec.write => return Err(write_outcome(c.spec, e, c.client_order_ids.clone())),
            Err(e) => e,
        };
        if !behavior::retry_read(may_retry(c.spec, &err), attempt) {
            return Err(err);
        }
        let delay = backoff.next_delay();
        c.hooks.emit(|| Event::Retry {
            socket: None,
            rpc: Some(c.spec.method.into()),
            attempt,
            delay,
            error: err.to_string(),
        });
        tokio::time::sleep(delay).await;
    }
}

async fn once(c: &RestCall<'_>) -> Result<Vec<u8>> {
    let r = &c.request;
    if debug_enabled() {
        let h: Vec<_> = r
            .headers
            .iter()
            .map(|(k, v)| format!("{k}: {}", redact_header(k, v)))
            .collect();
        tracing::debug!(target: "grvt", "POST {} headers={:?} body={}", r.url, h, redact(&r.body, 2000));
    }
    if Instant::now() >= c.deadline {
        return Err(Error::transport(
            "call deadline exceeded before dispatch",
            Some(c.spec.method),
            false,
        ));
    }
    c.dispatch.store(1, Ordering::Release);
    let resp = c
        .http
        .post(&r.url, r.body.clone(), &r.headers, c.timeout)
        .await
        .map_err(|e| match e {
            Error::Transport { message, sent, .. } => Error::Transport {
                message,
                sent,
                rpc: Some(c.spec.method.into()),
            },
            other => other,
        })?;
    if debug_enabled() {
        tracing::debug!(target: "grvt", "{} -> {} {}", c.spec.method, resp.status, redact(&resp.body, 2000));
    }
    c.encoding.rest_response_body(c.spec, resp.status, &resp.body)?;
    Ok(resp.body)
}
