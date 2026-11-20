//! Runs a test in both wire encodings. Users get lite by default and full is the other setting, so a test that
//! runs in one leaves the other unchecked.
//!
//! - `each` runs a test body once per encoding and names the encodings that failed.
//! - `serve` sets a `FakeHttp` handler written with full names and makes it speak the client's encoding, the
//!   way `FakeGateway` does for WebSocket connections on `/ws/lite`.
#![allow(dead_code)]

use std::future::Future;

use grvt::runtime::testing::FakeHttp;
use grvt::runtime::HttpResponse;
use grvt::Encoding;
use serde_json::Value;

use crate::lite_names::LiteNames;

pub const ALL: [Encoding; 2] = [Encoding::Full, Encoding::Lite];

/// Runs `test` once per encoding, each in its own task so one failing still runs the other. The panic of each
/// failed run is printed above the final one, which lists the encodings that failed.
pub async fn each<F, Fut>(test: F)
where
    F: Fn(Encoding) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut failed = Vec::new();
    for encoding in ALL {
        if tokio::spawn(test(encoding)).await.is_err() {
            failed.push(encoding);
        }
    }
    assert!(failed.is_empty(), "failed in encoding {failed:?}; see the panic above");
}

/// A gateway URL in its full form (`/lite/v1/...` -> `/full/v1/...`), so a test can match paths once for both.
pub fn as_full(url: &str) -> String {
    url.replacen("/lite/v", "/full/v", 1)
}

fn names() -> &'static LiteNames {
    static NAMES: std::sync::OnceLock<LiteNames> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| LiteNames::new(crate::fixtures::contract()))
}

/// Sets `handler` on `http`. The handler sees full names whatever the client sends: the URL as `as_full`, and in
/// lite the request body renamed back to full names (a key that is not a lite name of its type panics). Its
/// reply goes back in lite on a lite call: a 200 body renamed by the RPC's response type, an error body by the
/// contract's lite error names. Edge calls (login) have no lite form and pass through unchanged.
pub fn serve(http: &FakeHttp, handler: impl Fn(&str, &[u8]) -> grvt::Result<HttpResponse> + Send + Sync + 'static) {
    http.set_handler(move |url, body| {
        let full_url = as_full(url);
        let lite = full_url != url;
        let rpc = crate::fixtures::contract()["rpcs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| full_url.ends_with(r["http_path"].as_str().unwrap()));
        let Some(rpc) = rpc.filter(|_| lite) else {
            return handler(url, body);
        };
        let sent: Value = serde_json::from_slice(body).unwrap();
        let sent = names()
            .to_full(rpc["request"].as_str().unwrap(), &sent)
            .unwrap_or_else(|e| panic!("{url}: non-lite key sent: {e}"));
        let mut reply = handler(&full_url, sent.to_string().as_bytes())?;
        let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(&reply.body) else {
            return Ok(reply);
        };
        let lite_body = if reply.status == 200 {
            names().to_lite_lenient(rpc["response"].as_str().unwrap(), &Value::Object(body))
        } else {
            let lite = &crate::fixtures::contract()["transport"]["lite"]["http_error_body"];
            body.into_iter()
                .map(|(k, v)| (lite[&k].as_str().unwrap_or(&k).to_owned(), v))
                .collect()
        };
        reply.body = lite_body.to_string().into_bytes();
        Ok(reply)
    });
}
