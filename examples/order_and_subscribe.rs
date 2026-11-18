//! Without the client: a signed create-order request and an order-book subscription, with a fake
//! transport in place of the application's HTTP and WebSocket code. No network, no async runtime.
//!
//! cargo run --example order_and_subscribe --no-default-features

use grvt::{
    enums::TimeInForce, models as m, ops, signing, BookMode, Environment, InstrumentMeta, MessageKind, Meta, OrderBook,
    PrivateKeySigner, RequestDefaults, RequestTable, RpcOp, SessionState, SignContext, StreamOp, Verdict,
};

/// Stands in for the application's HTTP client.
fn fake_http(_request: &grvt::HttpRequest) -> (u16, Vec<u8>) {
    (
        400,
        br#"{"code":2064,"message":"Order price is not a multiple of the tick size","status":400}"#.to_vec(),
    )
}

fn main() -> grvt::Result<()> {
    let env = Environment::testnet();

    // Metadata the application loaded once (get_instrument -> signing::instrument_meta).
    let mut metadata = Meta::default();
    metadata.instruments.insert(
        "BTC_USDT_Perp".into(),
        InstrumentMeta {
            instrument_hash: "0x030501".into(),
            base_decimals: 9,
            quote_decimals: 6,
            // The order checks read these; a missing limit is not checked. signing::instrument_meta fills all three.
            limits: [("tick_size", "0.1"), ("min_size", "0.001"), ("min_notional", "10")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        },
    );

    // A published test key; for a KMS or wallet, implement grvt::SyncSigner.
    let key = PrivateKeySigner::new("0xbfe25ff07bd38cb3fbc28a2a296d41aabd68a78c929d257935906489fc9c6d89")?;

    // The order: every required value goes through a constructor.
    let leg =
        m::OrderLeg::new("BTC_USDT_Perp", "0.01".parse().unwrap(), true).with_limit_price("60000".parse().unwrap());
    let order = m::OrderInput::new(TimeInForce::GoodTillTime, Some(vec![leg])).with_post_only(true);
    let mut req = m::ApiCreateOrderRequestInput::new(order);

    // Fill the sub-account and client order id, then sign.
    let defaults = RequestDefaults {
        sub_account_id: Some(8289849667772468),
        client_order_ids: None,
    };
    let ids = grvt::prepare_request(ops::CreateOrder::SPEC, &mut req, &defaults)?;
    println!("needs {:?}", signing::needs_order_create_order(&req).instruments);
    signing::sign_order_create_order(&mut req, &SignContext::new(&key, &metadata, env.chain_id))?;

    // A real session comes from api_key_login_request + SessionState::on_login_response.
    let session = SessionState {
        cookie: Some("example".into()),
        ..Default::default()
    };
    let http = grvt::rest_request::<ops::CreateOrder>(&env, &req, Some(&session))?;
    println!("{} {}\n{}", http.method, http.url, String::from_utf8_lossy(&http.body));

    let (status, body) = fake_http(&http);
    if let Err(e) = grvt::rest_response::<ops::CreateOrder>(status, &body, &ids) {
        let retry = grvt::may_retry(ops::CreateOrder::SPEC, &e);
        println!("rejected: {e} (retry: {retry})");
        for name in grvt::stale_instruments(ops::CreateOrder::SPEC, &e, &http.body) {
            println!("reload metadata for {name}");
        }
    }

    // A write whose connection dropped is never resent: its outcome is unknown.
    println!(
        "{}",
        grvt::transport_failure(ops::CreateOrder::SPEC, true, "connection reset", ids)
    );

    // The order-book subscription: subscribe frame, then the reply and feed frames through a Sequencer.
    let mut requests = RequestTable::default();
    let selector = m::WSOrderbookLevelsFeedSelectorV1::new("BTC_USDT_Perp").with_rate(500);
    let selector = ops::OrderbookDeltaStream::selector(&selector);
    let id = requests
        .add(grvt::runtime::Pending::new("subscribe", false, "book"))
        .expect("free request id");
    println!(
        "{}",
        grvt::subscribe_frame(ops::OrderbookDeltaStream::SPEC, &selector, id)
    );

    let frames = [
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"stream":"v1.book.d","subs":["BTC_USDT_Perp@500-50"],"num_snapshots":["1"],"latest_sequence_number":["100"]}}}}"#
        ),
        r#"{"stream":"v1.book.d","selector":"BTC_USDT_Perp@500-50","sequence_number":"0","prev_sequence_number":"0","feed":{"instrument":"BTC_USDT_Perp","event_time":"1","bids":[{"price":"60000","size":"1","num_orders":1}],"asks":[{"price":"60001","size":"2","num_orders":1}]}}"#.into(),
        r#"{"stream":"v1.book.d","selector":"BTC_USDT_Perp@500-50","sequence_number":"101","prev_sequence_number":"100","feed":{"instrument":"BTC_USDT_Perp","event_time":"2","bids":[{"price":"60000","size":"0","num_orders":0}],"asks":null}}"#.into(),
    ];
    let mut seq = grvt::Sequencer::default();
    let mut book = OrderBook::new("BTC_USDT_Perp", BookMode::Delta);
    for text in &frames {
        // One parse per frame: route from it, then decode only the payload.
        let Ok(msg) = grvt::parse_ws_message(text.as_bytes()) else {
            continue;
        };
        match msg.kind {
            MessageKind::Reply => {
                let Some(pending) = msg.id.and_then(|id| requests.pop(id)) else {
                    continue;
                };
                let ack = grvt::parse_subscribe_result(grvt::ws_result(&msg, &pending.method)?, &selector)?;
                seq.on_ack(ack.latest_sequence_number, ack.num_snapshots);
            }
            MessageKind::Feed => {
                if seq.on_frame(msg.sequence_number, msg.prev_sequence_number) != Verdict::Deliver {
                    continue;
                }
                // A delivered frame that cannot be decoded is a lost frame: resubscribe (not shown).
                let Ok(frame) = grvt::decode_feed::<ops::OrderbookDeltaStream>(&msg) else {
                    continue;
                };
                let applied = if frame.sequence_number == 0 {
                    book.reset(&frame)
                } else {
                    book.apply(&frame)
                };
                if applied.is_ok() {
                    let snap = book.snapshot();
                    println!(
                        "book seq {:?}: {} bids, {} asks",
                        snap.as_ref().map(|s| s.sequence_number),
                        snap.as_ref().map_or(0, |s| s.bids.len()),
                        snap.as_ref().map_or(0, |s| s.asks.len())
                    );
                }
            }
            MessageKind::Other => {}
        }
    }
    // On disconnect: fail what was pending (writes become OrderStateUnknown), then reconnect with a Backoff.
    for failed in requests.connection_lost("socket closed") {
        println!("{}", failed.error);
    }
    assert!(book.ready());
    Ok(())
}
