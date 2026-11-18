//! Test doubles: an in-memory `HttpTransport` that records calls and answers login.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;

use super::error::Result;
use super::http::{HttpResponse, HttpTransport};

type Handler = Arc<dyn Fn(&str, &[u8]) -> Result<HttpResponse> + Send + Sync>;

/// One recorded request.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub url: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

/// Answers `/auth/api_key/login` with a `gravity` cookie and everything else with `200 {}` (or a
/// handler you set). Cheap to clone; clones share the record.
#[derive(Clone, Default)]
pub struct FakeHttp {
    calls: Arc<Mutex<Vec<Recorded>>>,
    handler: Arc<Mutex<Option<Handler>>>,
}

impl FakeHttp {
    pub fn set_handler(&self, f: impl Fn(&str, &[u8]) -> Result<HttpResponse> + Send + Sync + 'static) {
        *self.handler.lock().unwrap() = Some(Arc::new(f));
    }

    pub fn calls(&self) -> Vec<Recorded> {
        self.calls.lock().unwrap().clone()
    }

    pub fn last_url(&self) -> String {
        self.calls
            .lock()
            .unwrap()
            .last()
            .map(|c| c.url.clone())
            .unwrap_or_default()
    }

    pub fn ok(body: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: vec![],
            body: body.as_bytes().to_vec(),
        }
    }
}

impl HttpTransport for FakeHttp {
    fn post<'a>(
        &'a self,
        url: &'a str,
        body: Vec<u8>,
        headers: &'a [(String, String)],
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<HttpResponse>> {
        self.calls.lock().unwrap().push(Recorded {
            url: url.to_owned(),
            body: body.clone(),
            headers: headers.to_vec(),
        });
        let out = if url.ends_with(super::auth::LOGIN_PATH) {
            Ok(HttpResponse {
                status: 200,
                headers: vec![
                    (
                        "Set-Cookie".into(),
                        "gravity=cookie-123; Path=/; Max-Age=3600; HttpOnly".into(),
                    ),
                    ("X-Grvt-Account-Id".into(), "ACC:1".into()),
                ],
                body: br#"{"status":"success","funding_account_address":"0xabc"}"#.to_vec(),
            })
        } else {
            let h = self.handler.lock().unwrap().clone();
            match h {
                Some(h) => h(url, &body),
                None => Ok(Self::ok("{}")),
            }
        };
        Box::pin(std::future::ready(out))
    }

    fn get<'a>(&'a self, url: &'a str, _timeout: Duration) -> BoxFuture<'a, Result<HttpResponse>> {
        self.calls.lock().unwrap().push(Recorded {
            url: url.to_owned(),
            body: vec![],
            headers: vec![],
        });
        Box::pin(std::future::ready(Ok(Self::ok(r#"{"server_time":1700000000000}"#))))
    }
}
