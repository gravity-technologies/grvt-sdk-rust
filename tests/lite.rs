//! The client with `Encoding::Lite` against the fake HTTP transport and the fake gateway: every request it
//! sends carries only the contract's lite names (renamed per type by `lite_names`, independently of the
//! SDK's generated codecs), replies in lite decode to the same models as in full, and edge login stays full.
#![cfg(feature = "client")]

mod fake_gateway;
mod fixtures;
mod lite_names;
mod shared;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use fake_gateway::FakeGateway;
use futures::StreamExt;
use grvt::runtime::clock::{FixedClock, NonceProvider};
use grvt::runtime::testing::FakeHttp;
use grvt::runtime::{HttpResponse, InstrumentMeta};
use grvt::{enums, models as m, specs, Config, Encoding, Error, Grvt, OrderTransport, PrivateKeySigner};
use lite_names::LiteNames;
use serde_json::{json, Value};

const TEST_KEY: &str = "0xbfe25ff07bd38cb3fbc28a2a296d41aabd68a78c929d257935906489fc9c6d89";

fn names() -> LiteNames {
    LiteNames::new(fixtures::contract())
}

/// The same nonce sequence for every client, so a full and a lite client sign identical orders.
#[derive(Default)]
struct Nonces(AtomicU32);

impl NonceProvider for Nonces {
    fn next_nonce(&self) -> u32 {
        7 + self.0.fetch_add(1, Ordering::Relaxed)
    }
}

fn order() -> m::OrderInput {
    m::OrderInput::default()
        .with_sub_account_id(8289849667772468)
        .with_time_in_force(enums::TimeInForce::GoodTillTime)
        .with_legs(vec![m::OrderLeg::default()
            .with_instrument("BTC_USDT_Perp")
            .with_size(rust_decimal::Decimal::from_str_exact("0.01").unwrap())
            .with_limit_price(rust_decimal::Decimal::from_str_exact("65000.1").unwrap())
            .with_is_buying_asset(true)])
        .with_metadata(m::OrderMetadataInput::default().with_client_order_id(42))
}

fn trading(env: grvt::Environment, http: FakeHttp, encoding: Encoding, transport: OrderTransport) -> Grvt {
    let cfg = Config::new(env)
        .http(http)
        .api_key("test-key")
        .signer(PrivateKeySigner::new(TEST_KEY).unwrap())
        .order_transport(transport)
        .encoding(encoding)
        .nonces(Nonces::default())
        .clock(FixedClock(1_700_000_000_000_000_000));
    let client = Grvt::new(cfg).unwrap();
    client.runtime().metadata().put_instrument(
        "BTC_USDT_Perp",
        InstrumentMeta {
            instrument_hash: "0x030501".into(),
            base_decimals: 9,
            ..Default::default()
        },
    );
    client
}

/// Answers gateway calls from the contract fixtures, in lite on `/lite/` paths; records the requests, renamed
/// back to full names (an error if one carries a non-lite key).
/// (URL, request body renamed to full names or why it could not be).
type Requests = Arc<Mutex<Vec<(String, Result<Value, String>)>>>;

fn lite_http(requests: Requests) -> FakeHttp {
    let http = FakeHttp::default();
    http.set_handler(move |url, body| {
        let rpc = fixtures::contract()["rpcs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| url.ends_with(r["route"].as_str().unwrap()))
            .cloned()
            .unwrap();
        let (req_t, resp_t) = (rpc["request"].as_str().unwrap(), rpc["response"].as_str().unwrap());
        let sent: Value = serde_json::from_slice(body).unwrap();
        let lite = url.contains("/lite/");
        let full = if lite { names().to_full(req_t, &sent) } else { Ok(sent) };
        requests.lock().unwrap().push((url.to_owned(), full));
        let reply = fixtures::response(rpc["ws_method"].as_str().unwrap());
        let reply = if lite { names().to_lite(resp_t, &reply) } else { reply };
        Ok(FakeHttp::ok(&reply.to_string()))
    });
    http
}

#[tokio::test]
async fn rest_read_uses_lite_path_body_and_reply() {
    let requests = Arc::default();
    let http = lite_http(Arc::clone(&requests));
    let lite = Grvt::new(Config::testnet().http(http.clone()).encoding(Encoding::Lite)).unwrap();
    let got = lite.get_instrument().instrument("BTC_USDT_Perp").await.unwrap();
    let (url, sent) = requests.lock().unwrap()[0].clone();
    assert_eq!(url, "https://market-data.testnet.grvt.io/lite/v1/instrument");
    assert_eq!(sent.unwrap(), json!({"instrument": "BTC_USDT_Perp"}));
    let raw: Value = serde_json::from_slice(&http.calls()[0].body).unwrap();
    assert_eq!(
        raw,
        json!({names().lite("ApiGetInstrumentRequest", "instrument"): "BTC_USDT_Perp"})
    );

    let want: m::ApiGetInstrumentResponse = serde_json::from_value(fixtures::response("v1/instrument")).unwrap();
    assert_eq!(got, want, "a lite reply decodes to the same model as the full one");
}

#[tokio::test]
async fn rest_error_body_is_read_in_lite() {
    let http = FakeHttp::default();
    let c = fixtures::contract();
    let body = c["transport"]["lite"]["error_example"].as_str().unwrap().to_owned();
    http.set_handler(move |_, _| {
        Ok(HttpResponse {
            status: 400,
            headers: vec![],
            body: body.clone().into_bytes(),
        })
    });
    let lite = Grvt::new(Config::testnet().http(http).encoding(Encoding::Lite)).unwrap();
    let err = lite.get_instrument().instrument("X").await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::Api {
                code: 1003,
                status: Some(400),
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn rest_order_matches_full_and_login_stays_full() {
    let mut bodies = Vec::new();
    for encoding in [Encoding::Full, Encoding::Lite] {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let http = lite_http(requests.clone());
        let client = trading(
            grvt::Environment::testnet(),
            http.clone(),
            encoding,
            OrderTransport::Rest,
        );
        client.create_order().order(order()).await.unwrap();
        let login = &http.calls()[0];
        assert!(login.url.ends_with("/auth/api_key/login"), "{}", login.url);
        let login_body: Value = serde_json::from_slice(&login.body).unwrap();
        assert!(
            login_body.get("api_key").is_some(),
            "edge login is always full: {login_body}"
        );
        let (url, sent) = requests.lock().unwrap().pop().unwrap();
        let path = specs::ORDER_CREATE_ORDER.path_for(encoding);
        assert!(url.ends_with(path), "{url}");
        bodies.push(sent.unwrap());
        client.close().await;
    }
    assert_eq!(bodies[0], bodies[1], "the lite order is the full order renamed");
}

#[tokio::test]
async fn rest_order_rejection_still_refreshes_metadata() {
    let http = FakeHttp::default();
    let code = specs::ORDER_CREATE_ORDER.refresh_metadata_on[0];
    let lite_body = json!({"c": code, "m": "stale", "s": 400}).to_string();
    http.set_handler(move |_, _| {
        Ok(HttpResponse {
            status: 400,
            headers: vec![],
            body: lite_body.clone().into_bytes(),
        })
    });
    let client = trading(grvt::Environment::testnet(), http, Encoding::Lite, OrderTransport::Rest);
    let err = client.create_order().order(order()).await.unwrap_err();
    assert_eq!(err.code(), Some(code), "{err:?}");
    let loads = Arc::new(AtomicU32::new(0));
    let l = loads.clone();
    let _ = client
        .runtime()
        .metadata()
        .instrument("BTC_USDT_Perp", || async move {
            l.fetch_add(1, Ordering::Relaxed);
            Ok(InstrumentMeta::default())
        })
        .await;
    assert_eq!(
        loads.load(Ordering::Relaxed),
        1,
        "the order's instrument was dropped from the cache"
    );
}

#[tokio::test]
async fn ws_order_entry_sends_only_lite_keys() {
    let mut sent = Vec::new();
    for encoding in [Encoding::Full, Encoding::Lite] {
        let gw = FakeGateway::start().await;
        let client = trading(gw.env(), FakeHttp::default(), encoding, OrderTransport::Ws);
        client.create_order().order(order()).await.unwrap();
        let (errors, paths, received) = gw.with(|s| (s.lite_errors.clone(), s.paths.clone(), s.received.clone()));
        assert!(errors.is_empty(), "{errors:?}");
        let want_path = if encoding == Encoding::Lite {
            "/ws/lite"
        } else {
            "/ws/full"
        };
        assert!(paths.iter().all(|p| p == want_path), "{paths:?}");
        let call = received.iter().find(|r| r["method"] == "v1/create_order").unwrap();
        sent.push(call["params"].clone());
        if encoding == Encoding::Lite {
            let raw = gw.with(|s| s.received_lite.last().cloned().unwrap());
            let order_key = names().lite("ApiCreateOrderRequest", "order");
            assert!(raw["p"][&order_key].is_object(), "{raw}");
        }
        client.close().await;
    }
    assert_eq!(
        sent[0], sent[1],
        "the lite order is the full order renamed, signature included"
    );
}

#[tokio::test]
async fn ws_stream_in_lite_decodes_like_full() {
    let mut frames = Vec::new();
    for encoding in [Encoding::Full, Encoding::Lite] {
        let gw = FakeGateway::start().await;
        let client = Grvt::new(Config::new(gw.env()).http(FakeHttp::default()).encoding(encoding)).unwrap();
        let mut book = client
            .orderbook_snap_stream()
            .instrument("X")
            .rate(500)
            .depth(10)
            .await
            .unwrap();
        let snapshot = book.next().await.unwrap().unwrap();
        gw.push("v1.book.s", book.selector(), None, None);
        let next = book.next().await.unwrap().unwrap();
        assert_eq!(next.sequence_number, 101);
        let subscribe = gw.subscribes()[0]["params"].clone();
        assert_eq!(subscribe["stream"], "v1.book.s");
        assert!(gw.with(|s| s.lite_errors.is_empty()));
        frames.push((snapshot, next));
        drop(book);
        client.close().await;
    }
    assert_eq!(frames[0], frames[1]);
}

#[test]
fn lite_is_the_default() {
    assert_eq!(Encoding::default(), Encoding::Lite);
    assert_eq!(Config::testnet().encoding, Encoding::Lite);
    let mut env = grvt::Environment::testnet();
    env.md_ws_url = "wss://example.com/stream".into();
    assert!(Grvt::new(Config::new(env)).is_err(), "the default needs a lite URL");
}

#[test]
fn a_custom_ws_url_without_the_full_path_has_no_lite_form() {
    let mut env = grvt::Environment::testnet();
    env.md_ws_url = "wss://example.com/stream".into();
    let err = Grvt::new(Config::new(env.clone()).encoding(Encoding::Lite)).unwrap_err();
    assert!(
        matches!(err, Error::Config(ref m) if m.contains("no lite URL")),
        "{err:?}"
    );
    assert!(
        Grvt::new(Config::new(env).encoding(Encoding::Full)).is_ok(),
        "full needs no lite URL"
    );
}
