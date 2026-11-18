//! WS manager and REST behaviour against an in-process fake gateway and a fake HTTP transport.
#![cfg(feature = "client")]

mod fake_gateway;
mod fixtures;
mod lite_names;
mod shared;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fake_gateway::{wait_until, FakeGateway};
use grvt::runtime::clock::FixedClock;
use grvt::runtime::testing::FakeHttp;
use grvt::runtime::{HttpResponse, InstrumentMeta};
use grvt::{enums, models as m, Config, Encoding, Error, Event, EventKind, Grvt, OrderTransport, PrivateKeySigner};

const TEST_KEY: &str = "0xbfe25ff07bd38cb3fbc28a2a296d41aabd68a78c929d257935906489fc9c6d89";

type Events = Arc<Mutex<Vec<Event>>>;

fn client(gw: &FakeGateway, f: impl FnOnce(Config) -> Config) -> (Grvt, Events) {
    let events: Events = Arc::default();
    let ev = events.clone();
    let cfg = Config::new(gw.env()).http(FakeHttp::default()).on_event(move |e| {
        if e.kind() != EventKind::Frame {
            ev.lock().unwrap().push(e.clone());
        }
    });
    (Grvt::new(f(cfg)).unwrap(), events)
}

#[tokio::test]
async fn handshake_rejection_is_fatal() {
    let gw = FakeGateway::start().await;
    gw.with(|s| s.reject_handshake = Some(401));
    let (client, _) = client(&gw, |c| c);
    let err = client.ticker_delta_stream().instrument("X").await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::Api {
                status: Some(401),
                retryable: false,
                ..
            }
        ),
        "{err:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        gw.with(|s| s.handshakes.len()),
        1,
        "no reconnect loop after an auth failure"
    );
    client.close().await;
}

// -- order entry ---------------------------------------------------------------------------------

fn order() -> m::OrderInput {
    m::OrderInput::default()
        .with_sub_account_id(8289849667772468)
        .with_time_in_force(enums::TimeInForce::GoodTillTime)
        .with_legs(vec![m::OrderLeg::default()
            .with_instrument("BTC_USDT_Perp")
            .with_size(rust_decimal::Decimal::from_str_exact("0.01").unwrap())
            .with_limit_price(rust_decimal::Decimal::from_str_exact("65000.1").unwrap())
            .with_is_buying_asset(true)])
}

fn trading(gw: &FakeGateway, http: FakeHttp) -> Grvt {
    let cfg = Config::new(gw.env())
        .encoding(Encoding::Full)
        .http(http)
        .api_key("test-key")
        .signer(PrivateKeySigner::new(TEST_KEY).unwrap())
        .order_transport(OrderTransport::Ws)
        .ws_request_timeout(Duration::from_millis(300))
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

#[tokio::test]
async fn ws_order_entry_signs_and_sends_once() {
    let gw = FakeGateway::start().await;
    let client = trading(&gw, FakeHttp::default());
    client.create_order().order(order()).await.unwrap();
    let sent = gw.with(|s| {
        s.received
            .iter()
            .find(|r| r["method"] == "v1/create_order")
            .cloned()
            .unwrap()
    });
    let o = &sent["params"]["order"];
    assert!(o["signature"]["r"].as_str().unwrap().starts_with("0x"));
    assert_eq!(o["sub_account_id"], "8289849667772468"); // 64-bit ints as strings
    assert!(o["metadata"]["client_order_id"].is_string());
    let hs = gw.with(|s| s.handshakes[0].clone());
    assert_eq!(hs.get("cookie").map(String::as_str), Some("gravity=cookie-123"));
    assert_eq!(hs.get("x-grvt-account-id").map(String::as_str), Some("ACC:1"));
    client.close().await;
}

#[tokio::test]
async fn ws_order_timeout_is_order_state_unknown_and_not_retried() {
    let gw = FakeGateway::start().await;
    gw.with(|s| {
        s.ignore_rpcs.insert("v1/create_order".into());
    });
    let client = trading(&gw, FakeHttp::default());
    let err = client.create_order().order(order()).await.unwrap_err();
    match err {
        Error::OrderStateUnknown {
            rpc,
            client_order_id: Some(_),
            source: Some(source),
            ..
        } => {
            assert_eq!(rpc, "v1/create_order");
            assert!(matches!(*source, Error::Transport { sent: true, .. }));
        }
        other => panic!("expected OrderStateUnknown, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(gw.calls("v1/create_order"), 1, "writes are never retried");
    client.close().await;
}

fn bulk_orders() -> Vec<m::OrderInput> {
    let with_builder = (order())
        .with_builder(format!("0x{}", "22".repeat(20)))
        .with_builder_fee(rust_decimal::Decimal::from_str_exact("0.001").unwrap())
        .with_metadata(m::OrderMetadataInput::default().with_client_order_id(42));
    vec![order(), with_builder]
}

#[tokio::test]
async fn ws_bulk_orders_sign_each_order() {
    let gw = FakeGateway::start().await;
    let client = trading(&gw, FakeHttp::default());
    client
        .bulk_orders()
        .sub_account_id(8289849667772468)
        .orders(bulk_orders())
        .await
        .unwrap();
    let sent = gw.with(|s| {
        s.received
            .iter()
            .find(|r| r["method"] == "v2/bulk_orders")
            .cloned()
            .unwrap()
    });
    let orders = sent["params"]["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 2);
    for o in orders {
        assert!(o["signature"]["r"].as_str().unwrap().starts_with("0x"));
        assert!(o["metadata"]["client_order_id"].is_string());
    }
    assert_ne!(
        orders[0]["signature"]["nonce"], orders[1]["signature"]["nonce"],
        "own nonce per order"
    );
    assert_eq!(orders[1]["metadata"]["client_order_id"], "42", "a caller's id is kept");
    assert_eq!(gw.calls("v2/bulk_orders"), 1);
    client.close().await;
}

#[tokio::test]
async fn ws_bulk_timeout_carries_every_client_order_id() {
    let gw = FakeGateway::start().await;
    gw.with(|s| {
        s.ignore_rpcs.insert("v2/bulk_orders".into());
    });
    let client = trading(&gw, FakeHttp::default());
    let err = client
        .bulk_orders()
        .sub_account_id(8289849667772468)
        .orders(bulk_orders())
        .await
        .unwrap_err();
    match &err {
        Error::OrderStateUnknown {
            client_order_id: None,
            client_order_ids,
            ..
        } => {
            assert_eq!(client_order_ids.len(), 2);
            assert_eq!(client_order_ids[1], 42);
            assert!(err.to_string().contains("client_order_ids=["), "{err}");
        }
        other => panic!("expected OrderStateUnknown with two ids, got {other:?}"),
    }
    assert_eq!(gw.calls("v2/bulk_orders"), 1, "writes are never retried");
    client.close().await;
}

// -- REST ----------------------------------------------------------------------------------------

#[tokio::test]
async fn before_send_runs_before_signing_and_required_fields_are_checked() {
    let gw = FakeGateway::start().await;
    let http = FakeHttp::default();
    http.set_handler(|url, _| Ok(FakeHttp::ok(&fixtures::response(url).to_string())));
    let client = Grvt::new(
        Config::new(gw.env())
            .encoding(Encoding::Full)
            .http(http.clone())
            .before_send(|call| {
                if call.rpc == "v1/instrument" {
                    call.params["instrument"] = "ETH_USDT_Perp".into();
                }
            }),
    )
    .unwrap();
    client.get_instrument().instrument("BTC_USDT_Perp").await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&http.calls()[0].body).unwrap();
    assert_eq!(body["instrument"], "ETH_USDT_Perp");

    let missing = client.get_instrument().await.unwrap_err();
    assert!(
        matches!(&missing, Error::Config(m) if m.contains("instrument is required")),
        "{missing:?}"
    );
    assert_eq!(http.calls().len(), 1, "nothing sent when a required field is missing");
}

#[tokio::test]
async fn limit_rejection_refetches_the_instrument_and_never_resends() {
    let gw = FakeGateway::start().await;
    let http = FakeHttp::default();
    http.set_handler(|url, _| {
        let mut instrument = fixtures::model("InstrumentDisplay");
        instrument["instrument_hash"] = serde_json::json!("0x030501");
        instrument["base_decimals"] = serde_json::json!(9);
        instrument["quote_decimals"] = serde_json::json!(6);
        instrument["tick_size"] = serde_json::json!("0.1");
        instrument["min_size"] = serde_json::json!("0");
        instrument["min_notional"] = serde_json::json!("0");
        let instrument_body = serde_json::json!({"result":instrument}).to_string();
        let body: &[u8] = if url.ends_with("/full/v1/instrument") {
            instrument_body.as_bytes()
        } else {
            br#"{"code":2064,"message":"Invalid limit price tick","status":400}"#
        };
        let status = if url.ends_with("/full/v1/instrument") { 200 } else { 400 };
        Ok(HttpResponse {
            status,
            headers: vec![],
            body: body.to_vec(),
        })
    });
    let client = Grvt::new(
        Config::new(gw.env())
            .encoding(Encoding::Full)
            .http(http.clone())
            .api_key("k")
            .signer(PrivateKeySigner::new(TEST_KEY).unwrap()),
    )
    .unwrap();
    let count = |path: &str| http.calls().iter().filter(|c| c.url.ends_with(path)).count();
    for i in 1..=2 {
        let err = client.create_order().order(order()).await.unwrap_err();
        assert_eq!(err.code(), Some(2064), "{err:?}");
        assert_eq!(count("/full/v1/create_order"), i, "one send per order");
        assert_eq!(count("/full/v1/instrument"), i, "metadata reloaded after the rejection");
    }
}

#[tokio::test]
async fn off_tick_order_is_refused_before_sending() {
    let gw = FakeGateway::start().await;
    let http = FakeHttp::default();
    let client = Grvt::new(
        Config::new(gw.env())
            .encoding(Encoding::Full)
            .http(http.clone())
            .api_key("k")
            .signer(PrivateKeySigner::new(TEST_KEY).unwrap()),
    )
    .unwrap();
    let limits = [("tick_size", "0.1"), ("min_size", "0.001"), ("min_notional", "100")];
    let btc = InstrumentMeta {
        instrument_hash: "0x030501".into(),
        base_decimals: 9,
        quote_decimals: 6,
        limits: limits.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    };
    client.runtime().metadata().put_instrument("BTC_USDT_Perp", btc);
    let mut o = order();
    o.legs.as_mut().unwrap()[0].limit_price = Some(rust_decimal::Decimal::from_str_exact("65000.13").unwrap());
    let err = client.create_order().order(o).await.unwrap_err();
    match err {
        Error::OrderOutOfBounds(e) => {
            assert_eq!(e.nearest, vec!["65000.1", "65000.2"]);
            assert_eq!(e.server_code, 2064);
        }
        other => panic!("{other:?}"),
    }
    assert!(http.calls().is_empty(), "nothing sent");
}

#[tokio::test]
async fn unknown_write_uses_ids_after_the_hook_changes_bulk_membership() {
    let gw = FakeGateway::start().await;
    let http = FakeHttp::default();
    http.set_handler(|_, _| {
        Err(Error::Transport {
            message: "lost".into(),
            rpc: None,
            sent: true,
        })
    });
    let c = Grvt::new(
        Config::new(gw.env())
            .http(http)
            .api_key("k")
            .signer(PrivateKeySigner::new(TEST_KEY).unwrap())
            .before_send(|call| {
                if call.rpc == "v1/create_order" {
                    call.params["order"]["metadata"]["client_order_id"] = "99".into();
                }
                if call.rpc == "v2/bulk_orders" {
                    call.params["orders"].as_array_mut().unwrap().truncate(1);
                    call.params["orders"][0]["metadata"]["client_order_id"] = "99".into();
                }
            }),
    )
    .unwrap();
    c.runtime().metadata().put_instrument(
        "BTC_USDT_Perp",
        InstrumentMeta {
            instrument_hash: "0x030501".into(),
            base_decimals: 9,
            ..Default::default()
        },
    );
    let single = c.create_order().order(order()).await.unwrap_err();
    let bulk = c
        .bulk_orders()
        .sub_account_id(1)
        .orders(vec![order(), order()])
        .await
        .unwrap_err();
    for e in [single, bulk] {
        assert!(
            matches!(e, Error::OrderStateUnknown { client_order_ids, source:Some(_), .. } if client_order_ids == [99])
        );
    }
    c.close().await;
}

#[tokio::test]
async fn malformed_write_error_resolves_waiter_without_waiting_for_timeout() {
    let gw = FakeGateway::start().await;
    gw.with(|s| {
        s.ignore_rpcs.insert("v1/create_order".into());
    });
    let client = trading(&gw, FakeHttp::default());
    let c2 = client.clone();
    let pending = tokio::spawn(async move { c2.create_order().order(order()).await });
    wait_until("order sent", || gw.calls("v1/create_order") == 1).await;
    let id = gw.with(|s| s.received.iter().find(|r| r["method"] == "v1/create_order").unwrap()["id"].clone());
    gw.send_raw(&serde_json::json!({"id": id, "error": {"code": 1003}}).to_string());
    let error = tokio::time::timeout(Duration::from_millis(100), pending)
        .await
        .expect("matching reply must resolve promptly")
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(error, Error::OrderStateUnknown { source: Some(source), .. } if matches!(*source, Error::Decode { .. }))
    );
    client.close().await;
}

#[tokio::test]
async fn total_deadline_blocks_a_write_after_a_synchronous_hook() {
    let http = FakeHttp::default();
    let c = Grvt::new(
        Config::new(grvt::Environment::testnet())
            .http(http.clone())
            .call_timeout(Duration::from_millis(1))
            .before_send(|_| std::thread::sleep(Duration::from_millis(5))),
    )
    .unwrap();
    let error = c.get_instrument().instrument("BTC_USDT_Perp").await.unwrap_err();
    assert!(matches!(error, Error::Transport { sent: false, .. }));
    assert!(http.calls().is_empty());
    c.close().await;
}

#[derive(Clone)]
struct PendingWriteHttp(FakeHttp);
impl grvt::runtime::http::HttpTransport for PendingWriteHttp {
    fn post<'a>(
        &'a self,
        url: &'a str,
        body: Vec<u8>,
        headers: &'a [(String, String)],
        timeout: Duration,
    ) -> futures::future::BoxFuture<'a, grvt::Result<HttpResponse>> {
        if url.ends_with("/create_order") {
            Box::pin(std::future::pending())
        } else {
            grvt::runtime::http::HttpTransport::post(&self.0, url, body, headers, timeout)
        }
    }
    fn get<'a>(
        &'a self,
        url: &'a str,
        timeout: Duration,
    ) -> futures::future::BoxFuture<'a, grvt::Result<HttpResponse>> {
        grvt::runtime::http::HttpTransport::get(&self.0, url, timeout)
    }
}

#[tokio::test]
async fn total_deadline_after_dispatch_keeps_final_ids_and_cause() {
    let c = Grvt::new(
        Config::new(grvt::Environment::testnet())
            .http(PendingWriteHttp(FakeHttp::default()))
            .api_key("key")
            .signer(PrivateKeySigner::new(TEST_KEY).unwrap())
            .before_send(|call| {
                if call.rpc == "v1/create_order" {
                    call.params["order"]["metadata"]["client_order_id"] = "99".into();
                }
            }),
    )
    .unwrap();
    c.runtime().metadata().put_instrument(
        "BTC_USDT_Perp",
        InstrumentMeta {
            instrument_hash: "0x030501".into(),
            base_decimals: 9,
            ..Default::default()
        },
    );
    let e = c
        .create_order()
        .order(order())
        .timeout(Duration::from_millis(20))
        .await
        .unwrap_err();
    assert!(
        matches!(e, Error::OrderStateUnknown{client_order_ids,source:Some(source),..}
        if client_order_ids==[99] && matches!(*source, Error::Transport{sent:true,..}))
    );
    c.close().await;
}

#[tokio::test]
async fn cancelled_metadata_initializer_does_not_fail_surviving_waiter() {
    let cache = Arc::new(grvt::runtime::MetadataCache::default());
    let started = Arc::new(tokio::sync::Notify::new());
    let first = {
        let cache = cache.clone();
        let started = started.clone();
        tokio::spawn(async move {
            cache
                .instrument("ETH", || async move {
                    started.notify_one();
                    std::future::pending::<grvt::Result<InstrumentMeta>>().await
                })
                .await
        })
    };
    started.notified().await;
    let second = {
        let cache = cache.clone();
        tokio::spawn(async move {
            cache
                .instrument("ETH", || async {
                    Ok(InstrumentMeta {
                        base_decimals: 9,
                        ..Default::default()
                    })
                })
                .await
        })
    };
    tokio::task::yield_now().await;
    first.abort();
    let value = tokio::time::timeout(Duration::from_secs(1), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(value.base_decimals, 9);
}

#[test]
fn zero_call_or_attempt_timeout_is_invalid_at_construction() {
    assert!(Grvt::new(Config::new(grvt::Environment::testnet()).call_timeout(Duration::ZERO)).is_err());
    assert!(Grvt::new(Config::new(grvt::Environment::testnet()).http_attempt_timeout(Duration::ZERO)).is_err());
}

#[tokio::test]
async fn custom_reqwest_builder_cannot_enable_redirect_replay() {
    use grvt::runtime::{HttpTransport, ReqwestTransport};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = accepted.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut request = [0; 4096];
            let count = stream.read(&mut request).await.unwrap();
            assert!(count > 0);
            let reply = format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{addr}/replayed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            stream.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    let transport =
        ReqwestTransport::with_builder(reqwest::Client::builder().redirect(reqwest::redirect::Policy::limited(5)))
            .unwrap();
    let response = transport
        .post(
            &format!("http://{addr}/write"),
            b"{}".to_vec(),
            &[],
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 307);
    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
    server.abort();
}
