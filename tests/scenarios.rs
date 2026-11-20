//! Replays sdk/scenarios/runtime.json, the runtime scenarios every SDK shares, against the fake gateway (WS)
//! and the fake HTTP transport (REST). The format is described in sdk/scenarios/README.md.
//!
//! A test with its own `main` (`harness = false` in Cargo.toml), so each scenario is reported as its own
//! test and a skipped one shows its reason: `cargo test --test scenarios [name filter]`.

mod fake_gateway;
mod fixtures;
mod lite_names;
mod shared;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fake_gateway::{wait_until, FakeGateway};
use futures::future::BoxFuture;
use futures::StreamExt;
use grvt::runtime::http::HttpTransport;
use grvt::runtime::testing::FakeHttp;
use grvt::runtime::{HttpResponse, InstrumentMeta};
use grvt::{
    enums, models as m, Config, Encoding, Error, Event, EventKind, GapRange, Grvt, OrderTransport, PrivateKeySigner,
    Subscription,
};
use serde_json::{json, Value};

const TEST_KEY: &str = "0xbfe25ff07bd38cb3fbc28a2a296d41aabd68a78c929d257935906489fc9c6d89";

/// runtime.json with its behavior.json references already replaced by their values (sdk/generator/scenarios).
fn scenarios() -> Vec<Value> {
    match shared::load("scenarios/runtime.resolved.json") {
        Value::Array(a) => a,
        _ => panic!("runtime.resolved.json is a list"),
    }
}

/// `SessionRotated` -> `session_rotated`, the kind's name in behavior.json.
fn snake(name: &str) -> String {
    name.chars()
        .enumerate()
        .flat_map(|(i, c)| {
            let sep = (i > 0 && c.is_uppercase()).then_some('_');
            sep.into_iter().chain(c.to_lowercase())
        })
        .collect()
}

/// The scripted replies in order; the last one repeats.
#[derive(Default)]
struct Script {
    replies: Vec<Value>,
    attempts: usize,
}

impl Script {
    fn next(&mut self) -> Value {
        self.attempts += 1;
        if self.replies.len() > 1 {
            self.replies.remove(0)
        } else {
            self.replies.first().cloned().unwrap_or(Value::Null)
        }
    }
}

fn names() -> &'static lite_names::LiteNames {
    static NAMES: std::sync::OnceLock<lite_names::LiteNames> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| lite_names::LiteNames::new(fixtures::contract()))
}

/// The contract RPC a REST URL calls, in either encoding.
fn rpc(url: &str) -> Option<&'static Value> {
    let full = url.replace("/lite/", "/full/");
    fixtures::contract()["rpcs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| full.ends_with(r["http_path"].as_str().unwrap()))
}

/// A reply the scenario wrote with full names, as the gateway sends it in lite: a 200 body renamed per the
/// RPC's response type, an error body by the contract's lite error names. Raw bodies go out as written.
fn reply_to_lite(rpc: &Value, mut r: Value) -> Value {
    if !r["body"].is_object() {
        return r;
    }
    r["body"] = if r["status"] == 200 {
        names().to_lite_lenient(rpc["response"].as_str().unwrap(), &r["body"])
    } else {
        let lite = &fixtures::contract()["transport"]["lite"]["http_error_body"];
        let body = r["body"].as_object().unwrap().iter();
        Value::Object(
            body.map(|(k, v)| (lite[k].as_str().unwrap_or(k).to_owned(), v.clone()))
                .collect(),
        )
    };
    r
}

fn http_reply(r: &Value) -> grvt::Result<HttpResponse> {
    if r["refuse"] == true || r["drop"] == true {
        return Err(Error::Transport {
            message: "connection lost".into(),
            rpc: None,
            sent: r["drop"] == true,
        });
    }
    let body = match r["raw"].as_str() {
        Some(raw) => raw.to_owned(),
        None => r["body"].to_string(),
    };
    Ok(HttpResponse {
        status: r["status"].as_u64().unwrap() as u16,
        headers: vec![],
        body: body.into_bytes(),
    })
}

/// An order at 65000.1, or off the 0.1 tick at 65000.15.
fn order(client_order_id: Option<u64>, off_tick: bool) -> m::OrderInput {
    let price = if off_tick { "65000.15" } else { "65000.1" };
    let o = m::OrderInput::default()
        .with_sub_account_id(8289849667772468)
        .with_time_in_force(enums::TimeInForce::GoodTillTime)
        .with_legs(vec![m::OrderLeg::default()
            .with_instrument("BTC_USDT_Perp")
            .with_size(rust_decimal::Decimal::from_str_exact("0.01").unwrap())
            .with_limit_price(rust_decimal::Decimal::from_str_exact(price).unwrap())
            .with_is_buying_asset(true)]);
    match client_order_id {
        Some(id) => o.with_metadata(m::OrderMetadataInput::default().with_client_order_id(id)),
        None => o,
    }
}

/// The scenario outcome of a result. Rust has one `Error::Config` for bad input and a missing setting: the
/// countdown's bounds check is the scenarios' validation_error, anything else a config_error.
fn outcome(result: &grvt::Result<()>, kind: &str) -> &'static str {
    match result {
        Ok(()) => "ok",
        Err(Error::OrderStateUnknown { .. }) => "order_state_unknown",
        Err(Error::Decode { .. }) => "decode_error",
        Err(Error::RateLimit { .. }) => "rate_limited",
        Err(Error::Api { .. }) => "api_error",
        Err(Error::Transport { .. }) => "transport_error",
        Err(Error::Config(_)) if kind == "countdown" => "validation_error",
        Err(Error::Config(_)) => "config_error",
        Err(Error::OrderOutOfBounds(_)) => "order_out_of_bounds",
        Err(_) => "unexpected",
    }
}

async fn run_call(sc: &Value, encoding: Encoding) {
    let (call, want) = (&sc["call"], &sc["expect"]);
    let ws = call["transport"] == "ws";
    let gw = FakeGateway::start().await;
    let script = Arc::new(Mutex::new(Script {
        replies: sc["replies"].as_array().unwrap().clone(),
        ..Default::default()
    }));
    let lite = encoding == Encoding::Lite;
    let bulk = call["bulk"] == true;
    // The RPC the script answers: its WebSocket method, and its REST path after /full or /lite.
    let method = match call["kind"].as_str().unwrap() {
        "read" => "v1/ticker",
        "write" if bulk => "v2/bulk_orders",
        "write" => "v1/create_order",
        _ => "v1/cancel_on_disconnect",
    };
    let path = format!("/{}/{method}", if lite { "lite" } else { "full" });
    // Gateway calls as (URL, body with full names); in lite, the keys that are not lite names.
    let requests: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let lite_errors: Arc<Mutex<Vec<String>>> = Arc::default();
    let http = FakeHttp::default();
    let (s, reqs, errs, p) = (script.clone(), requests.clone(), lite_errors.clone(), path.clone());
    http.set_handler(move |url, body| {
        let Some(rpc) = rpc(url) else {
            return Ok(FakeHttp::ok("{}"));
        };
        let mut sent: Value = serde_json::from_slice(body).unwrap();
        if lite {
            match names().to_full(rpc["request"].as_str().unwrap(), &sent) {
                Ok(full) => sent = full,
                Err(e) => errs.lock().unwrap().push(e),
            }
        }
        reqs.lock().unwrap().push((url.to_owned(), sent));
        if url.ends_with(&p) {
            let reply = s.lock().unwrap().next();
            return http_reply(&if lite { reply_to_lite(rpc, reply) } else { reply });
        }
        let reply = fixtures::response(rpc["ws_method"].as_str().unwrap());
        let reply = if lite {
            names().to_lite(rpc["response"].as_str().unwrap(), &reply)
        } else {
            reply
        };
        Ok(FakeHttp::ok(&reply.to_string()))
    });
    let events: Arc<Mutex<Vec<Event>>> = Arc::default();
    let ev = events.clone();
    let mut cfg = Config::new(gw.env())
        .encoding(encoding)
        .http(http.clone())
        .on_event(move |e| ev.lock().unwrap().push(e.clone()))
        .signer(PrivateKeySigner::new(TEST_KEY).unwrap());
    if call["api_key"] != false {
        cfg = cfg.api_key("test-key");
    }
    if ws {
        cfg = cfg.order_transport(OrderTransport::Ws);
        gw.with(|s| s.ignore_rpcs.insert(method.into()));
    }
    if let Some(ms) = call["ws_request_timeout_ms"].as_u64() {
        cfg = cfg.ws_request_timeout(Duration::from_millis(ms));
    }
    let client = Grvt::new(cfg).unwrap();
    client.runtime().metadata().put_instrument(
        "BTC_USDT_Perp",
        InstrumentMeta {
            instrument_hash: "0x030501".into(),
            base_decimals: 9,
            limits: [("tick_size".to_owned(), "0.1".to_owned())].into(),
            ..Default::default()
        },
    );

    let c = client.clone();
    let client_order_id = call["client_order_id"].as_str().map(|id| id.parse::<u64>().unwrap());
    let kind = call["kind"].as_str().unwrap().to_owned();
    let seconds = call["seconds"].as_f64().unwrap_or(0.0) as u16;
    let off_tick = call["off_tick"] == true;
    let task = tokio::spawn(async move {
        match kind.as_str() {
            "read" => c.ticker().instrument("BTC_USDT_Perp").await.map(|_| ()),
            "countdown" => grvt::refresh_cancel_countdown(&c, 123, seconds).await.map(|_| ()),
            _ if bulk => c
                .bulk_orders()
                .sub_account_id(8289849667772468)
                .orders(vec![order(None, false), order(None, false)])
                .await
                .map(|_| ()),
            _ => c
                .create_order()
                .order(order(client_order_id, off_tick))
                .await
                .map(|_| ()),
        }
    });
    let sent_ws = |gw: &FakeGateway| -> Vec<Value> {
        gw.with(|s| s.received.iter().filter(|r| r["method"] == method).cloned().collect())
    };
    if ws {
        let reply = script.lock().unwrap().next();
        wait_until("write sent", || gw.calls(method) == 1).await;
        if reply["drop"] == true {
            gw.drop_all();
        } else if !reply["error"].is_null() {
            let id = sent_ws(&gw)[0]["id"].clone();
            gw.send_raw(&json!({"jsonrpc": "2.0", "id": id, "error": reply["error"]}).to_string());
        }
    }
    let result = task.await.unwrap();

    assert_eq!(
        outcome(&result, call["kind"].as_str().unwrap()),
        want["outcome"],
        "{result:?}"
    );
    match &result {
        Err(Error::Api {
            code,
            status,
            retryable,
            ..
        }) => {
            assert_eq!(Some(u64::from(*code)), want["code"].as_u64(), "code");
            if let Some(s) = want.get("status") {
                assert_eq!(&json!(status), s, "status");
            }
            if let Some(r) = want["retryable"].as_bool() {
                assert_eq!(*retryable, r, "retryable");
            }
        }
        Err(Error::RateLimit { status, .. }) => {
            if let Some(s) = want.get("status") {
                assert_eq!(&json!(status), s, "status");
            }
        }
        Err(Error::OrderStateUnknown { rpc, .. }) => {
            if let Some(r) = want["rpc"].as_str() {
                assert_eq!(rpc, r, "rpc");
            }
        }
        _ => {}
    }
    if let Some(n) = want["retries"].as_u64() {
        let retries = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.kind() == EventKind::Retry)
            .count();
        assert_eq!(retries as u64, n, "retry events");
    }
    if let Some(wanted) = want["events"].as_array() {
        // Each event as the scenario names it, with only the fields an expectation may list.
        let got: Vec<Value> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.kind() != EventKind::Frame)
            .map(|e| match e {
                Event::Retry { rpc, attempt, .. } => json!({"kind": "retry", "rpc": rpc, "attempt": attempt}),
                e => json!({ "kind": snake(&format!("{:?}", e.kind())) }),
            })
            .collect();
        assert_eq!(got.len(), wanted.len(), "events {got:?}");
        for (g, w) in got.iter().zip(wanted) {
            for (k, v) in w.as_object().unwrap() {
                assert_eq!(&g[k], v, "event {k} in {g}");
            }
        }
    }
    if let (Err(Error::Transport { sent, .. }), Some(want_sent)) = (&result, want["sent"].as_bool()) {
        assert_eq!(*sent, want_sent);
    }
    if let (Err(Error::OrderStateUnknown { client_order_ids, .. }), Some(ids)) =
        (&result, want["client_order_ids"].as_array())
    {
        let got: Vec<Value> = client_order_ids.iter().map(|id| json!(id.to_string())).collect();
        assert_eq!(&got, ids);
    }
    let sent: Vec<Value> = if ws {
        sent_ws(&gw).into_iter().map(|r| r["params"].clone()).collect()
    } else {
        let requests = requests.lock().unwrap();
        requests
            .iter()
            .filter(|(url, _)| url.ends_with(&path))
            .map(|(_, b)| b.clone())
            .collect()
    };
    if let Some(n) = want["attempts"].as_u64() {
        assert_eq!(sent.len() as u64, n, "attempts");
    }
    // The client order IDs of each sent request's orders, in order.
    let sent_ids = |body: &Value| -> Vec<Value> {
        let orders = if bulk {
            body["orders"].as_array().unwrap().clone()
        } else {
            vec![body["order"].clone()]
        };
        orders
            .iter()
            .map(|o| o["metadata"]["client_order_id"].clone())
            .collect()
    };
    if let Err(Error::OrderStateUnknown { client_order_ids, .. }) = &result {
        let got: Vec<Value> = client_order_ids.iter().map(|id| json!(id.to_string())).collect();
        assert_eq!(got, sent_ids(sent.last().unwrap()), "reported vs sent client order ids");
    }
    match want["sent_client_order_id"].as_str() {
        Some("generated") => {
            for id in sent_ids(&sent[0]) {
                let id: u64 = id.as_str().unwrap().parse().unwrap();
                assert!(id >= 1 << 63, "generated client order id {id} is under 2^63");
            }
        }
        Some(id) => assert_eq!(sent_ids(&sent[0]), [id]),
        None => {}
    }
    if lite {
        let (ws_errors, paths) = gw.with(|s| (s.lite_errors.clone(), s.paths.clone()));
        assert!(ws_errors.is_empty(), "non-lite keys sent over WS: {ws_errors:?}");
        assert!(
            lite_errors.lock().unwrap().is_empty(),
            "non-lite keys sent over REST: {lite_errors:?}"
        );
        assert!(paths.iter().all(|p| p.ends_with("/ws/lite")), "{paths:?}");
        let full: Vec<String> = http
            .calls()
            .into_iter()
            .map(|c| c.url)
            .filter(|u| u.contains("/full/"))
            .collect();
        assert!(full.is_empty(), "full REST paths in lite: {full:?}");
    }
    client.close().await;
}

/// One subscription of a stream scenario, whatever its feed type.
enum Sub {
    Delta(Subscription<m::PartialWSTickerFeedDataV1>),
    Snap(Subscription<m::WSTickerFeedDataV1>),
    Order(Subscription<m::WSOrderFeedDataV1>),
}

impl Sub {
    fn dropped(&self) -> u64 {
        match self {
            Sub::Delta(s) => s.dropped(),
            Sub::Snap(s) => s.dropped(),
            Sub::Order(s) => s.dropped(),
        }
    }

    /// The next item: its sequence number, "decode_error", "stream_overflow" (with the capacity it reports)
    /// or the error text.
    async fn next_item(&mut self) -> Option<(Value, Option<usize>)> {
        fn item<F>(r: grvt::Result<F>, seq: impl Fn(&F) -> u64) -> (Value, Option<usize>) {
            match r {
                Ok(f) => (json!(seq(&f)), None),
                Err(Error::Decode { .. }) => (json!("decode_error"), None),
                Err(Error::StreamOverflow { capacity, .. }) => (json!("stream_overflow"), Some(capacity)),
                Err(e) => (json!(e.to_string()), None),
            }
        }
        match self {
            Sub::Delta(s) => s.next().await.map(|r| item(r, |f| f.sequence_number)),
            Sub::Snap(s) => s.next().await.map(|r| item(r, |f| f.sequence_number)),
            Sub::Order(s) => s.next().await.map(|r| item(r, |f| f.sequence_number)),
        }
    }
}

/// The fake HTTP transport, with logins that hand out numbered cookies ("cookie-<n>"): the first lasts
/// `first_max_age` seconds, later ones an hour.
#[derive(Clone)]
struct SessionHttp {
    inner: FakeHttp,
    logins: Arc<Mutex<Vec<Instant>>>,
    first_max_age: u64,
}

impl HttpTransport for SessionHttp {
    fn post<'a>(
        &'a self,
        url: &'a str,
        body: Vec<u8>,
        headers: &'a [(String, String)],
        timeout: Duration,
    ) -> BoxFuture<'a, grvt::Result<HttpResponse>> {
        if !url.ends_with("/auth/api_key/login") {
            return self.inner.post(url, body, headers, timeout);
        }
        let mut logins = self.logins.lock().unwrap();
        logins.push(Instant::now());
        let n = logins.len();
        let max_age = if n == 1 { self.first_max_age } else { 3600 };
        Box::pin(std::future::ready(Ok(HttpResponse {
            status: 200,
            headers: vec![
                (
                    "Set-Cookie".into(),
                    format!("gravity=cookie-{n}; Path=/; Max-Age={max_age}; HttpOnly"),
                ),
                ("X-Grvt-Account-Id".into(), "ACC:1".into()),
            ],
            body: br#"{"status":"success","funding_account_address":"0xabc"}"#.to_vec(),
        })))
    }

    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> BoxFuture<'a, grvt::Result<HttpResponse>> {
        self.inner.get(url, timeout)
    }
}

async fn run_stream(sc: &Value, encoding: Encoding) {
    let (call, want) = (&sc["call"], &sc["expect"]);
    let stream = call["stream"].as_str().unwrap();
    let gw = FakeGateway::start().await;
    let http = SessionHttp {
        inner: FakeHttp::default(),
        logins: Arc::default(),
        first_max_age: call["cookie_max_age_s"].as_u64().unwrap_or(3600),
    };
    let events: Arc<Mutex<Vec<Event>>> = Arc::default();
    let ev = events.clone();
    let mut cfg = Config::new(gw.env())
        .http(http.clone())
        .api_key("test-key")
        .on_event(move |e| {
            if e.kind() != EventKind::Frame {
                ev.lock().unwrap().push(e.clone());
            }
        });
    cfg = cfg.encoding(encoding);
    if let Some(ms) = call["resync_timeout_ms"].as_u64() {
        cfg = cfg.resync_timeout(Duration::from_millis(ms));
    }
    if let Some(ms) = call["idle_timeout_ms"].as_u64() {
        cfg = cfg.idle_timeout(Duration::from_millis(ms));
    }
    if let Some(n) = call["queue_size"].as_u64() {
        cfg = cfg.queue_size(n as usize);
    }
    let client = Grvt::new(cfg).unwrap();
    let count = |k: EventKind| events.lock().unwrap().iter().filter(|e| e.kind() == k).count();
    let instruments: Vec<String> = match call["instruments"].as_array() {
        Some(a) => a.iter().map(|i| i.as_str().unwrap().to_owned()).collect(),
        None => vec!["BTC_USDT_Perp".into()],
    };
    let (mut subs, mut selectors) = (Vec::new(), Vec::new());
    if stream == "v1.order" {
        subs.push(Sub::Order(client.order_stream().sub_account_id(42).await.unwrap()));
        selectors.push("42".to_owned());
    }
    for i in instruments.iter().filter(|_| stream != "v1.order") {
        subs.push(match stream {
            "v1.ticker.d" => Sub::Delta(client.ticker_delta_stream().instrument(i).rate(500).await.unwrap()),
            _ => Sub::Snap(client.ticker_snap_stream().instrument(i).rate(500).await.unwrap()),
        });
        selectors.push(format!("{i}@500"));
    }
    let selector = selectors[0].clone();
    for step in sc["steps"].as_array().unwrap() {
        if let Some(p) = step["push"].as_array() {
            let (seq, prev) = (p[0].as_u64().unwrap(), p[1].as_u64().unwrap());
            match step.get("feed") {
                None => gw.push(stream, &selector, Some(seq), Some(prev)),
                Some(feed) => gw.send_raw(
                    &json!({"stream": stream, "selector": selector, "sequence_number": seq.to_string(),
                            "prev_sequence_number": prev.to_string(), "feed": feed["value"]})
                    .to_string(),
                ),
            }
        } else if step["drop"] == true {
            gw.drop_all();
        } else if step["silence"] == true {
            gw.silence_all();
        } else if let Some(ms) = step["sleep_ms"].as_u64() {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        } else if let Some(n) = step["ignore_subscribes"].as_u64() {
            gw.with(|s| s.ignore_subscribes = n as usize);
        } else if let Some(n) = step["ack_latest"].as_u64() {
            gw.with(|s| s.ack_latest = Some(n));
        } else if let Some(n) = step["wait_acks"].as_u64() {
            wait_until("acks", || count(EventKind::Subscribe) >= n as usize).await;
        } else if let Some(n) = step["wait_reconnects"].as_u64() {
            wait_until("reconnects", || count(EventKind::Reconnect) >= n as usize).await;
        } else if let Some(n) = step["wait_dropped"].as_u64() {
            wait_until("dropped items", || subs[0].dropped() >= n).await;
        }
    }
    // Each subscription in order: the first's expectations, then other_subscriptions.
    let mut wanted = vec![want.clone()];
    wanted.extend(want["other_subscriptions"].as_array().cloned().unwrap_or_default());
    assert_eq!(wanted.len(), subs.len(), "one expectation per subscription");
    for ((sub, selector), w) in subs.iter_mut().zip(&selectors).zip(&wanted) {
        let mut items = Vec::new();
        for _ in w["items"].as_array().unwrap() {
            let item = tokio::time::timeout(Duration::from_secs(5), sub.next_item()).await;
            let (item, capacity) = item.expect("stream items in time").expect("stream open");
            if let Some(capacity) = capacity {
                assert_eq!(
                    Some(capacity as u64),
                    want["overflow_capacity"].as_u64(),
                    "overflow capacity"
                );
            }
            items.push(item);
        }
        assert_eq!(&Value::Array(items), &w["items"], "items of {selector}");
        let gaps: Vec<Value> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::Gap {
                    gap,
                    selector: s,
                    expected_prev,
                    prev,
                    seq,
                    ..
                } if s == selector => Some((gap.clone(), *expected_prev, *prev, *seq)),
                _ => None,
            })
            .enumerate()
            .map(|(i, (gap, expected_prev, prev, seq))| {
                let mut g = json!({"reason": gap.reason, "after": gap.after_sequence, "through": gap.through_sequence});
                // the legacy seq and prev, when the data gives them; the legacy expected prev is after
                if !w["gaps"][i]["frame"].is_null() && Some(expected_prev) == gap.after_sequence {
                    g["frame"] = json!([seq, prev]);
                }
                g
            })
            .collect();
        assert_eq!(&Value::Array(gaps), &w["gaps"], "gaps of {selector}");
    }
    for s in &selectors {
        let n = gw
            .subscribes()
            .iter()
            .filter(|r| r["params"]["selectors"].as_array().unwrap().contains(&json!(s)))
            .count();
        assert_eq!(Some(n as u64), want["subscribes"].as_u64(), "subscribes for {s}");
    }
    if !want["subscribe_request"].is_null() {
        assert_eq!(
            gw.subscribes()[0]["params"],
            want["subscribe_request"],
            "subscribe request"
        );
    }
    if let Some(n) = want["unsubscribes"].as_u64() {
        drop(subs.remove(0));
        wait_until("unsubscribes", || gw.calls("unsubscribe") as u64 >= n).await;
        assert_eq!(gw.calls("unsubscribe") as u64, n, "unsubscribes");
    }
    let logins = http.logins.lock().unwrap().clone();
    if let Some(n) = want["logins"].as_u64() {
        assert_eq!(logins.len() as u64, n, "logins");
    }
    if let Some(r) = want["relogin_after_ms"].as_array() {
        let after = (logins[1] - logins[0]).as_secs_f64() * 1000.0;
        let (low, high) = (r[0].as_f64().unwrap(), r[1].as_f64().unwrap());
        assert!(
            low <= after && after <= high,
            "re-login after {after}ms, want within {r:?}"
        );
    }
    if let Some(generations) = want["session_rotated"].as_array() {
        let got: Vec<Value> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::SessionRotated {
                    websocket_reconnect_required,
                    generation,
                    ..
                } => {
                    assert!(
                        websocket_reconnect_required,
                        "session_rotated without a WebSocket reconnect"
                    );
                    Some(json!(generation))
                }
                _ => None,
            })
            .collect();
        assert_eq!(&got, generations, "session_rotated generations");
    }
    if let Some(n) = want["handshake_logins"].as_array() {
        let got: Vec<Value> = gw.with(|s| {
            s.handshakes
                .iter()
                .map(|h| {
                    json!(h["cookie"]
                        .trim_start_matches("gravity=cookie-")
                        .parse::<u64>()
                        .unwrap())
                })
                .collect()
        });
        assert_eq!(&got, n, "handshakes carried the cookies of these logins");
    }
    if encoding == Encoding::Lite {
        let (errors, paths) = gw.with(|s| (s.lite_errors.clone(), s.paths.clone()));
        assert!(errors.is_empty(), "non-lite keys sent: {errors:?}");
        assert!(paths.iter().all(|p| p.ends_with("/ws/lite")), "{paths:?}");
    }
    drop(subs);
    client.close().await;
}

async fn run_pages(sc: &Value) {
    let (call, want) = (&sc["call"], &sc["expect"]);
    let script = Arc::new(Mutex::new(Script {
        replies: sc["replies"].as_array().unwrap().clone(),
        ..Default::default()
    }));
    let cursors: Arc<Mutex<Vec<Value>>> = Arc::default();
    let (seen, s) = (cursors.clone(), script.clone());
    let fetch = move |cursor: Option<String>| {
        seen.lock().unwrap().push(json!(cursor));
        let reply = s.lock().unwrap().next();
        async move {
            match reply["next"].as_str() {
                Some(next) => Ok(m::ApiFillHistoryResponse::default().with_result(vec![]).with_next(next)),
                None => Err(Error::Config("lookup failed".into())),
            }
        }
    };
    let take = call["take"].as_u64().map_or(usize::MAX, |n| n as usize);
    let gap = call["gap"].as_object().map(|g| GapRange {
        after_sequence: g["after"].as_u64(),
        through_sequence: g["through"].as_u64(),
        reason: match g["reason"].as_str().unwrap() {
            "disconnect" => "disconnect",
            "overflow" => "overflow",
            _ => "sequence_mismatch",
        },
    });
    // Plain pages yield None, backfill pages Some(page).
    type Pages = std::pin::Pin<
        Box<dyn futures::Stream<Item = grvt::Result<Option<grvt::BackfillPage<m::ApiFillHistoryResponse>>>> + Send>,
    >;
    let mut pages: Pages = match &gap {
        None => Box::pin(grvt::pages(fetch, None).map(|r| r.map(|_| None))),
        Some(gap) => Box::pin(grvt::backfill_pages(fetch, gap.clone()).map(|r| r.map(Some))),
    };
    assert!(
        cursors.lock().unwrap().is_empty(),
        "fetched before the first page was read"
    );
    let (mut read, mut exhausted, mut err) = (0, Vec::new(), None);
    while read < take {
        match pages.next().await {
            Some(Ok(page)) => {
                read += 1;
                if let Some(page) = page {
                    exhausted.push(json!(page.history_exhausted));
                    assert_eq!(page.continuity(), want["continuity"], "continuity");
                    assert_eq!(Some(page.gap), gap, "a page keeps the gap it was read for");
                }
            }
            Some(Err(e)) => {
                err = Some(e);
                break;
            }
            None => break,
        }
    }
    let got = match &err {
        None => "ok",
        Some(Error::PaginationCycle { cursor }) => {
            assert_eq!(Some(cursor.as_str()), want["cursor"].as_str(), "cycle cursor");
            "pagination_cycle"
        }
        Some(Error::Config(m)) if m == "lookup failed" => "fetch_error",
        Some(e) => panic!("unexpected {e:?}"),
    };
    assert_eq!(got, want["outcome"], "outcome");
    assert_eq!(Some(read as u64), want["pages"].as_u64(), "pages");
    assert_eq!(
        &Value::Array(cursors.lock().unwrap().clone()),
        &want["cursors"],
        "cursors"
    );
    if !want["history_exhausted"].is_null() {
        assert_eq!(Value::Array(exhausted), want["history_exhausted"], "history exhausted");
    }
}

/// The libtest arguments this runner reads: name filters (any matches; none means all) and `--skip`
/// filters. Values of the flags that take one are not filters.
fn filters(args: &[String]) -> (Vec<String>, Vec<String>) {
    const TAKES_VALUE: [&str; 6] = ["--test-threads", "--skip", "--color", "--format", "--logfile", "-Z"];
    let (mut keep, mut skip) = (Vec::new(), Vec::new());
    let mut args = args.iter();
    while let Some(a) = args.next() {
        if a == "--skip" {
            skip.extend(args.next().cloned());
        } else if TAKES_VALUE.contains(&a.as_str()) {
            args.next();
        } else if !a.starts_with('-') {
            keep.push(a.clone());
        }
    }
    (keep, skip)
}

/// Runs every selected scenario as its own test.
fn main() {
    let strings = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        filters(&strings(&[
            "--test-threads",
            "1",
            "--color=never",
            "-Z",
            "x",
            "--skip",
            "ws",
            "gap",
            "--exact"
        ])),
        (strings(&["gap"]), strings(&["ws"])),
        "argument parsing"
    );
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (keep, skip) = filters(&args);
    // Call and stream scenarios run in both encodings ("<name> [lite]" against lite fakes).
    let selected: Vec<(String, Value, Encoding)> = scenarios()
        .into_iter()
        .flat_map(|sc| {
            let name = sc["name"].as_str().unwrap().to_owned();
            let lite = matches!(
                sc["call"]["kind"].as_str(),
                Some("read" | "write" | "countdown" | "stream")
            )
            .then(|| (format!("{name} [lite]"), sc.clone(), Encoding::Lite));
            std::iter::once((name, sc, Encoding::Full)).chain(lite)
        })
        .filter(|(name, _, _)| {
            (keep.is_empty() || keep.iter().any(|f| name.contains(f.as_str())))
                && !skip.iter().any(|f| name.contains(f.as_str()))
        })
        .collect();
    if args.iter().any(|a| a == "--list") {
        for (name, _, _) in &selected {
            println!("{name}: test");
        }
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (mut passed, mut failed, mut ignored) = (0, Vec::new(), 0);
    println!("\nrunning {} tests", selected.len());
    for (name, sc, encoding) in selected {
        if let Some(reason) = sc["skip"]["rust"].as_str() {
            println!("test {name} ... ignored, {reason}");
            ignored += 1;
            continue;
        }
        let run = rt.spawn(async move {
            match sc["call"]["kind"].as_str().unwrap() {
                "stream" => run_stream(&sc, encoding).await,
                "pages" | "backfill" => run_pages(&sc).await,
                _ => run_call(&sc, encoding).await,
            }
        });
        if rt.block_on(run).is_ok() {
            println!("test {name} ... ok");
            passed += 1;
        } else {
            println!("test {name} ... FAILED");
            failed.push(name);
        }
    }
    let result = if failed.is_empty() { "ok" } else { "FAILED" };
    println!(
        "\ntest result: {result}. {passed} passed; {} failed; {ignored} ignored\n",
        failed.len()
    );
    if !failed.is_empty() {
        println!("failures:\n    {}\n", failed.join("\n    "));
        std::process::exit(1);
    }
}
