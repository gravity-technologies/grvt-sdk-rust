//! The no-I/O request and reply functions: REST and WS requests, reply and frame parsing, error
//! classification, transport failures, request defaults, the session state machine and the recovery
//! helpers. Nothing here opens a socket.

use grvt::runtime::{RandomNonce, TimeCounterClientOrderId};
use grvt::{
    error_codes, models as m, ops, Environment, Error, GapRange, MessageKind, RequestDefaults, RpcOp, SessionState,
    StreamOp,
};

fn logged_in() -> SessionState {
    let mut s = SessionState::default();
    s.on_login_response(
        200,
        &[
            ("Set-Cookie".into(), "gravity=c-1; Path=/; Max-Age=3600".into()),
            ("X-Grvt-Account-Id".into(), "ACC:1".into()),
        ],
        b"{}",
        1000.0,
    )
    .unwrap();
    s
}

fn order() -> m::ApiCreateOrderRequestInput {
    let leg =
        m::OrderLeg::new("BTC_USDT_Perp", "0.01".parse().unwrap(), true).with_limit_price("60000".parse().unwrap());
    m::ApiCreateOrderRequestInput::new(m::OrderInput::new(
        grvt::enums::TimeInForce::GoodTillTime,
        Some(vec![leg]),
    ))
}

#[test]
fn rest_request_and_reply() {
    let env = Environment::testnet();
    let req = m::ApiGetInstrumentRequest::new("BTC_USDT_Perp");
    let http = grvt::rest_request::<ops::GetInstrument>(&env, &req, None).unwrap();
    assert_eq!(http.method, "POST");
    assert_eq!(http.url, "https://market-data.testnet.grvt.io/full/v1/instrument");
    assert_eq!(http.body, br#"{"instrument":"BTC_USDT_Perp"}"#);
    assert_eq!(
        http.headers,
        vec![("Content-Type".to_owned(), "application/json".to_owned())]
    );

    // An authenticated call carries the session, and refuses to build without one.
    let mut req = order();
    let missing = grvt::rest_request::<ops::CreateOrder>(&env, &req, None).unwrap_err();
    assert!(matches!(missing, Error::Config(m) if m.contains("needs a session")));
    let ids = grvt::prepare_request(
        ops::CreateOrder::SPEC,
        &mut req,
        &RequestDefaults {
            sub_account_id: Some(42),
            client_order_ids: None,
        },
    )
    .unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(req.order.sub_account_id, Some(42));
    assert_eq!(req.order.metadata.as_ref().unwrap().client_order_id, Some(ids[0]));
    let http = grvt::rest_request::<ops::CreateOrder>(&env, &req, Some(&logged_in())).unwrap();
    assert!(http.url.ends_with("/full/v1/create_order"));
    assert!(http.headers.contains(&("Cookie".to_owned(), "gravity=c-1".to_owned())));
    assert!(http
        .headers
        .contains(&("X-Grvt-Account-Id".to_owned(), "ACC:1".to_owned())));
    assert!(!format!("{http:?}").contains("c-1"), "Debug redacts the cookie");

    // Replies: a 200 body decodes, an error body is classified.
    let ok = grvt::rest_response::<ops::GetInstrument>(200, br#"{"result":{"instrument":"BTC_USDT_Perp"}}"#, &[]);
    assert!(
        matches!(ok, Err(Error::Decode { .. })),
        "an incomplete instrument does not decode"
    );
    let rejected = grvt::rest_response::<ops::CreateOrder>(
        400,
        br#"{"code":2064,"message":"Order price is not a multiple of the tick size","status":400}"#,
        &[],
    )
    .unwrap_err();
    assert!(matches!(
        rejected,
        Error::Api {
            code: 2064,
            status: Some(400),
            retryable: false,
            ..
        }
    ));
    assert!(!grvt::may_retry(ops::CreateOrder::SPEC, &rejected));
    let body = serde_json::to_vec(&req).unwrap();
    assert_eq!(
        grvt::stale_instruments(ops::CreateOrder::SPEC, &rejected, &body),
        vec!["BTC_USDT_Perp"]
    );
    let limited =
        grvt::rest_response::<ops::GetInstrument>(429, br#"{"code":1006,"message":"slow"}"#, &[]).unwrap_err();
    assert!(matches!(limited, Error::RateLimit { .. }));
    assert!(grvt::may_retry(ops::GetInstrument::SPEC, &limited));
    assert!(
        !grvt::may_retry(ops::CreateOrder::SPEC, &limited),
        "writes are never retried"
    );
    let gateway = grvt::rest_response::<ops::GetInstrument>(503, b"<html>bad gateway</html>", &[]).unwrap_err();
    assert!(
        matches!(&gateway, Error::Api { code: 0, retryable: true, message, .. } if message.contains("bad gateway"))
    );
}

#[test]
fn sdk_client_order_ids_use_the_api_client_range() {
    use grvt::runtime::ClientOrderIdProvider;
    let ids = TimeCounterClientOrderId::default();
    let (a, b) = (ids.next_id(), ids.next_id());
    assert!(a >= 1 << 63 && b >= 1 << 63 && a != b, "{a} {b}");
}

#[test]
fn required_fields_are_checked_before_anything_is_built() {
    let mut req = m::ApiGetOrderRequest::default();
    let err = grvt::prepare_request(ops::GetOrder::SPEC, &mut req, &RequestDefaults::default()).unwrap_err();
    assert!(matches!(err, Error::Config(m) if m == "v1/order: sub_account_id is required"));
    let ids = TimeCounterClientOrderId::default();
    let defaults = RequestDefaults {
        sub_account_id: Some(7),
        client_order_ids: Some(&ids),
    };
    assert!(grvt::prepare_request(ops::GetOrder::SPEC, &mut req, &defaults)
        .unwrap()
        .is_empty());
    assert_eq!(req.sub_account_id, 7);
}

#[test]
fn transport_failures_never_hide_a_write() {
    let lost = grvt::transport_failure(ops::CreateOrder::SPEC, true, "connection reset", vec![11]);
    assert!(matches!(
        &lost,
        Error::OrderStateUnknown { client_order_id: Some(11), rpc, reason, .. } if rpc == "v1/create_order" && reason.contains("connection reset")
    ));
    let bulk = grvt::transport_failure(ops::BulkOrders::SPEC, true, "timeout", vec![1, 2]);
    assert!(
        matches!(&bulk, Error::OrderStateUnknown { client_order_id: None, client_order_ids, .. } if client_order_ids == &[1, 2])
    );
    let unsent = grvt::transport_failure(ops::CreateOrder::SPEC, false, "connect refused", vec![11]);
    assert!(matches!(unsent, Error::Transport { sent: false, .. }));
    let read = grvt::transport_failure(ops::GetInstrument::SPEC, true, "timeout", Vec::new());
    assert!(matches!(&read, Error::Transport { sent: true, .. }));
    assert!(grvt::may_retry(ops::GetInstrument::SPEC, &read));
}

#[test]
fn ws_request_and_reply() {
    let req = m::ApiGetInstrumentRequest::new("BTC_USDT_Perp");
    assert_eq!(
        grvt::ws_request::<ops::GetInstrument>(&req, 9).unwrap(),
        r#"{"jsonrpc":"2.0","method":"v1/instrument","params":{"instrument":"BTC_USDT_Perp"},"id":9}"#
    );
    let reply = grvt::parse_ws_message(br#"{"jsonrpc":"2.0","id":9,"result":{"result":{"ack":true}}}"#).unwrap();
    assert_eq!((reply.kind, reply.id), (MessageKind::Reply, Some(9)));
    let ack = grvt::ws_response::<ops::CancelOnDisconnect>(&reply, &[]).unwrap();
    assert!(ack.result.ack);
    let err = grvt::parse_ws_message(br#"{"jsonrpc":"2.0","id":9,"error":{"code":2064,"message":"tick"}}"#).unwrap();
    assert!(matches!(
        grvt::ws_response::<ops::CreateOrder>(&err, &[]),
        Err(Error::Api { code: 2064, status: None, rpc: Some(r), .. }) if r == "v1/create_order"
    ));
}

/// The `_into` forms write the same bytes as `ws_request` and `rest_request`, into a buffer that is reused.
#[test]
fn encode_into_kept_buffer() {
    let env = Environment::testnet();
    let mut req = order();
    req.order.sub_account_id = Some(8289849667772468);
    let mut buf = b"left over".to_vec();
    for id in [9, u32::MAX] {
        grvt::ws_request_into::<ops::CreateOrder>(&req, id, &mut buf).unwrap();
        assert_eq!(buf, grvt::ws_request::<ops::CreateOrder>(&req, id).unwrap().as_bytes());
    }
    grvt::encode_request_into(&req, &mut buf).unwrap();
    assert_eq!(
        buf,
        grvt::rest_request::<ops::CreateOrder>(&env, &req, Some(&logged_in()))
            .unwrap()
            .body
    );
}

#[test]
fn stream_subscribe_and_frames() {
    let sel = m::WSOrderbookLevelsFeedSelectorV1::new("BTC_USDT_Perp")
        .with_rate(500)
        .with_depth(10);
    assert_eq!(ops::OrderbookDeltaStream::selector(&sel), "BTC_USDT_Perp@500-10");
    let frame = grvt::subscribe_frame(ops::OrderbookDeltaStream::SPEC, "BTC_USDT_Perp@500-10", 3);
    let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(v["method"], "subscribe");
    assert_eq!(v["params"]["stream"], "v1.book.d");
    assert_eq!(v["params"]["selectors"][0], "BTC_USDT_Perp@500-10");
    assert_eq!(v["params"]["use_global_sequence_number"], true);
    let un: serde_json::Value = serde_json::from_str(&grvt::unsubscribe_frame(
        ops::OrderbookDeltaStream::SPEC,
        "BTC_USDT_Perp@500-10",
        4,
    ))
    .unwrap();
    assert_eq!(un["method"], "unsubscribe");

    let ack = grvt::parse_ws_message(
        br#"{"jsonrpc":"2.0","id":3,"result":{"stream":"v1.book.d","subs":["BTC_USDT_Perp@500-10"],"num_snapshots":["1"],"latest_sequence_number":["100"]}}"#,
    )
    .unwrap();
    let ack =
        grvt::parse_subscribe_result(grvt::ws_result(&ack, "subscribe").unwrap(), "BTC_USDT_Perp@500-10").unwrap();
    assert_eq!((ack.latest_sequence_number, ack.num_snapshots), (100, 1));

    let text = br#"{"stream":"v1.book.d","selector":"BTC_USDT_Perp@500-10","sequence_number":"101","prev_sequence_number":"100","feed":{"instrument":"BTC_USDT_Perp","event_time":"5","bids":[{"price":"60000","size":"1","num_orders":2}],"asks":[]}}"#;
    let msg = grvt::parse_ws_message(text).unwrap();
    assert_eq!(msg.kind, MessageKind::Feed);
    assert_eq!((msg.sequence_number, msg.prev_sequence_number), (Some(101), Some(100)));
    let feed = grvt::decode_feed::<ops::OrderbookDeltaStream>(&msg).unwrap();
    assert_eq!(
        feed,
        serde_json::from_slice::<m::WSOrderbookLevelsFeedDataV1>(text).unwrap(),
        "same as decoding the whole frame"
    );
    assert_eq!(feed.selector, "BTC_USDT_Perp@500-10");
    assert_eq!(feed.feed.bids.as_ref().unwrap()[0].num_orders, 2);

    // The sequencer delivers in order and flags the gap.
    let mut seq = grvt::Sequencer::default();
    seq.on_ack(ack.latest_sequence_number, ack.num_snapshots);
    assert_eq!(seq.on_frame(Some(0), None), grvt::Verdict::Deliver);
    assert_eq!(
        seq.on_frame(msg.sequence_number, msg.prev_sequence_number),
        grvt::Verdict::Deliver
    );
    assert_eq!(seq.on_frame(Some(105), Some(103)), grvt::Verdict::Gap);

    // A frame missing a required routing field does not decode; a broken payload is a decode error.
    let no_seq = grvt::parse_ws_message(br#"{"stream":"v1.book.d","selector":"X","feed":{}}"#).unwrap();
    assert!(
        matches!(grvt::decode_feed::<ops::OrderbookDeltaStream>(&no_seq), Err(Error::Decode { rpc, .. }) if rpc == "v1.book.d")
    );
    let bad = grvt::parse_ws_message(
        br#"{"stream":"v1.book.d","selector":"X","sequence_number":"1","prev_sequence_number":"0","feed":{"bids":7}}"#,
    )
    .unwrap();
    assert!(grvt::decode_feed::<ops::OrderbookDeltaStream>(&bad).is_err());
}

#[test]
fn login_and_session_headers() {
    let env = Environment::testnet();
    let login = grvt::api_key_login_request(&env, "key-1");
    assert_eq!(login.url, "https://edge.testnet.grvt.io/auth/api_key/login");
    let mut s = SessionState::default();
    assert!(s.headers().is_empty() && !s.fresh(0.0));
    let denied = s
        .on_login_response(403, &[], br#"{"error":"forbidden"}"#, 0.0)
        .unwrap_err();
    assert!(matches!(
        denied,
        Error::Api {
            retryable: false,
            status: Some(403),
            ..
        }
    ));
    let s = logged_in();
    assert_eq!((s.generation, s.issued_at, s.expires_at), (1, 1000.0, 4600.0));
    assert!(s.fresh(1000.0 + 0.79 * 3600.0) && !s.fresh(1000.0 + 0.8 * 3600.0));
}

#[test]
fn recovery_and_countdown_steps() {
    assert!(grvt::resolve_order_request(0, 1).is_err());
    let req = grvt::resolve_order_request(5, 9).unwrap();
    assert_eq!((req.sub_account_id, req.client_order_id), (5, Some(9)));
    let not_found = Err(grvt::api_error(
        error_codes::NOT_FOUND,
        "not found",
        Some(404),
        Some("v1/order"),
    ));
    assert!(matches!(
        grvt::resolve_order_result(5, 9, not_found).unwrap(),
        grvt::OrderResolution::StillUnknown
    ));
    let other = Err(grvt::api_error(error_codes::INTERNAL_SERVER, "boom", Some(500), None));
    assert!(grvt::resolve_order_result(5, 9, other).is_err());

    let arm = grvt::cancel_countdown_request(5, 10).unwrap();
    assert_eq!(arm.countdown_time, Some(10_000));
    assert!(grvt::cancel_countdown_request(5, 301).is_err());

    let gap = GapRange {
        after_sequence: Some(10),
        through_sequence: None,
        reason: "disconnect",
    };
    struct P(Option<&'static str>);
    impl grvt::Page for P {
        fn next_cursor(&self) -> Option<&str> {
            self.0
        }
    }
    let status = grvt::backfill_status(&gap, &P(None));
    assert!(status.history_exhausted);
    assert_eq!(status.continuity(), "unproven");

    let mut walk = grvt::PageCursor::new(None);
    assert_eq!(walk.next_page().unwrap(), grvt::PageStep::Fetch(None));
    walk.advance(&P(Some("a")));
    assert_eq!(walk.next_page().unwrap(), grvt::PageStep::Fetch(Some("a".to_owned())));
    walk.advance(&P(Some("a")));
    assert!(matches!(walk.next_page(), Err(Error::PaginationCycle { .. })));
    let mut done = grvt::PageCursor::new(None);
    done.next_page().unwrap();
    done.advance(&P(Some("")));
    assert_eq!(done.next_page().unwrap(), grvt::PageStep::Done);
}

#[test]
fn request_table_fails_lost_writes() {
    use grvt::runtime::Pending;
    let mut table = grvt::RequestTable::default();
    let mut write = Pending::new("v1/create_order", true, ());
    write.client_order_ids = vec![3];
    table.add(write).unwrap();
    let failed = table.connection_lost("socket closed");
    assert!(matches!(
        &failed[0].error,
        Error::OrderStateUnknown {
            client_order_id: Some(3),
            ..
        }
    ));
    let _ = RandomNonce;
}

#[test]
fn ws_message_edge_cases() {
    // whitespace before the object, keys in any order, escaped strings unescaped
    let msg = grvt::parse_ws_message(
        b" \n\t{\"feed\":{\"instrument\":\"X\"},\"prev_sequence_number\":\"1\",\"selector\":\"X\\u0040500\",\"sequence_number\":\"2\",\"stream\":\"v1.book.d\"}",
    )
    .unwrap();
    assert_eq!(msg.kind, MessageKind::Feed);
    assert_eq!(msg.selector.as_deref(), Some("X@500"));
    assert_eq!(msg.feed, Some(r#"{"instrument":"X"}"#));
    // arrays and other non-objects are not frames
    for bad in [&b"[5,{\"result\":1}]"[..], b"5", b"\"x\"", b"", b"   "] {
        assert!(
            grvt::parse_ws_message(bad).is_err(),
            "{:?}",
            String::from_utf8_lossy(bad)
        );
    }
    // Keep the reply id so the matching waiter fails promptly. A malformed error
    // remains a decode failure rather than success or a known business rejection.
    for raw in [
        br#"{"id":1,"error":"boom"}"#.as_slice(),
        br#"{"id":1,"error":{"message":"boom"}}"#.as_slice(),
        br#"{"id":1,"error":{"code":1003}}"#.as_slice(),
    ] {
        let reply = grvt::parse_ws_message(raw).unwrap();
        assert_eq!(reply.id, Some(1));
        assert!(matches!(grvt::ws_result(&reply, "write"), Err(Error::Decode { .. })));
    }
}

#[test]
fn uncertain_write_replies_keep_all_ids_and_the_original_error() {
    for (status, body) in [
        (200, "null"),
        (200, "not json"),
        (200, "{}"),
        (400, "proxy error"),
        (400, r#"{"code":1003.5,"message":"bad"}"#),
        (400, r#"{"code":1003}"#),
        (503, r#"{"code":1006,"message":"busy"}"#),
    ] {
        let e = grvt::rest_response::<ops::CreateOrder>(status, body.as_bytes(), &[4, 5]).unwrap_err();
        assert!(
            matches!(&e, Error::OrderStateUnknown { client_order_ids, source: Some(_), .. } if client_order_ids == &[4, 5]),
            "{status}: {e:?}"
        );
    }
    for code in [1002, 1014] {
        let frame = format!(r#"{{"id":1,"error":{{"code":{code},"message":"server failure"}}}}"#);
        let reply = grvt::parse_ws_message(frame.as_bytes()).unwrap();
        assert!(matches!(
            grvt::ws_response::<ops::CreateOrder>(&reply, &[4, 5]),
            Err(Error::OrderStateUnknown { source: Some(_), .. })
        ));
    }
    let msg = grvt::parse_ws_message(br#"{"id":1,"result":null}"#).unwrap();
    let e = grvt::ws_response::<ops::CreateOrder>(&msg, &[4, 5]).unwrap_err();
    assert!(
        matches!(e, Error::OrderStateUnknown { client_order_ids, source: Some(_), .. } if client_order_ids == [4, 5])
    );
}
