//! Full vs lite wire encoding (no criterion; plain loops, median of 7 runs). `cargo bench --bench lite`.
//!
//! - an order book snapshot frame (10 and 100 levels a side) through the stream path: parse once, route,
//!   decode the payload
//! - a large REST response: get_all_instruments with 200 instruments
//! - encoding a create_order WS request
//!
//! The lite bodies come from the full ones renamed with the contract's lite names (tests/lite_names), so
//! both decode to the same models; each pair is checked equal before timing.

#[path = "../tests/lite_names/mod.rs"]
mod lite_names;
#[path = "../tests/shared/mod.rs"]
mod shared;

use std::hint::black_box;
use std::time::Instant;

use grvt::runtime::StreamOp;
use grvt::{enums, models as m, ops, Encoding, RpcOp};
use serde_json::{json, Value};

fn bench(name: &str, iters: u32, mut f: impl FnMut()) -> f64 {
    for _ in 0..iters / 10 {
        f();
    }
    let mut runs: Vec<f64> = (0..7)
        .map(|_| {
            let t = Instant::now();
            for _ in 0..iters {
                f();
            }
            t.elapsed().as_nanos() as f64 / iters as f64
        })
        .collect();
    runs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let ns = runs[3];
    if ns >= 1000.0 {
        println!("{name:<52} {:>9.2} µs/op", ns / 1000.0);
    } else {
        println!("{name:<52} {:>9.0} ns/op", ns);
    }
    ns
}

fn pair(name: &str, iters: u32, full: impl FnMut(), lite: impl FnMut()) {
    let f = bench(&format!("{name} (full)"), iters, full);
    let l = bench(&format!("{name} (lite)"), iters, lite);
    println!("{:<52} {:>+8.1} %", "  lite vs full", (l / f - 1.0) * 100.0);
}

fn book_frame(levels: usize) -> Value {
    let level = |p: f64, i: usize| json!({"price": format!("{p:.1}"), "size": format!("{}.{:03}", i % 7, (i * 37) % 1000), "num_orders": 1 + i % 9});
    let bids: Vec<_> = (0..levels).map(|i| level(65000.0 - i as f64 * 0.1, i)).collect();
    let asks: Vec<_> = (0..levels).map(|i| level(65000.1 + i as f64 * 0.1, i)).collect();
    json!({
        "stream": "v1.book.s", "selector": format!("BTC_USDT_Perp@500-{levels}"), "sequence_number": "8178232896400000",
        "prev_sequence_number": "8178232869400000",
        "feed": {"event_time": "1794947226500005600", "instrument": "BTC_USDT_Perp", "bids": bids, "asks": asks},
    })
}

fn all_instruments(names: &lite_names::LiteNames, count: usize) -> Value {
    let mut one = names.sample("InstrumentDisplay");
    for (k, v) in [
        ("base", "BTC"),
        ("quote", "USDT"),
        ("instrument_hash", "0x030501"),
        ("tick_size", "0.1"),
        ("min_size", "0.001"),
        ("create_time", "1700000000000000000"),
        ("max_position_size", "1000.5"),
        ("funding_interval_hours", "8"),
        ("adjusted_funding_rate_cap", "0.3"),
        ("adjusted_funding_rate_floor", "-0.3"),
        ("min_notional", "10"),
    ] {
        if one.get(k).is_some_and(|old| old.is_string()) {
            one[k] = json!(v);
        }
    }
    let list: Vec<_> = (0..count)
        .map(|i| {
            let mut x = one.clone();
            x["instrument"] = json!(format!("ASSET{i}_USDT_Perp"));
            x
        })
        .collect();
    json!({ "result": list })
}

fn order_request() -> m::ApiCreateOrderRequestInput {
    m::ApiCreateOrderRequestInput::default().with_order(
        m::OrderInput::default()
            .with_sub_account_id(8289849667772468)
            .with_time_in_force(enums::TimeInForce::GoodTillTime)
            .with_post_only(true)
            .with_legs(vec![m::OrderLeg::default()
                .with_instrument("BTC_USDT_Perp")
                .with_size(rust_decimal::Decimal::from_str_exact("1.5").unwrap())
                .with_limit_price(rust_decimal::Decimal::from_str_exact("65000.5").unwrap())
                .with_is_buying_asset(true)])
            .with_metadata(m::OrderMetadataInput::default().with_client_order_id(1001))
            .with_signature(
                m::SignatureInput::default()
                    .with_signer(format!("0x{}", "ab".repeat(20)))
                    .with_r(format!("0x{}", "cd".repeat(32)))
                    .with_s(format!("0x{}", "ef".repeat(32)))
                    .with_v(28)
                    .with_nonce(7)
                    .with_expiration(grvt::Timestamp::from_nanoseconds(1_700_000_000_000_000_000)),
            ),
    )
}

fn decode_frame(encoding: Encoding, text: &[u8]) -> m::WSOrderbookLevelsFeedDataV1 {
    let msg = encoding.parse_ws_message(text).unwrap();
    grvt::decode_feed::<ops::OrderbookSnapStream>(&msg).unwrap()
}

fn main() {
    let names = lite_names::LiteNames::new(&shared::load("contract/contract.json"));
    let feed_t = "WSOrderbookLevelsFeedDataV1";
    for levels in [10, 100] {
        let full = book_frame(levels);
        let lite = names.to_lite(feed_t, &full);
        let (full, lite) = (full.to_string().into_bytes(), lite.to_string().into_bytes());
        assert_eq!(decode_frame(Encoding::Full, &full), decode_frame(Encoding::Lite, &lite));
        println!(
            "\nbook snapshot, {levels} levels a side: {} B full, {} B lite ({:+.0} %)",
            full.len(),
            lite.len(),
            (lite.len() as f64 / full.len() as f64 - 1.0) * 100.0
        );
        assert_eq!(ops::OrderbookSnapStream::SPEC.stream, "v1.book.s");
        pair(
            &format!("parse + decode book frame ({levels}+{levels})"),
            if levels == 10 { 200_000 } else { 20_000 },
            || {
                black_box(decode_frame(Encoding::Full, black_box(&full)));
            },
            || {
                black_box(decode_frame(Encoding::Lite, black_box(&lite)));
            },
        );
    }

    let full = all_instruments(&names, 200);
    let lite = names.to_lite("ApiGetAllInstrumentsResponse", &full);
    let (full, lite) = (full.to_string().into_bytes(), lite.to_string().into_bytes());
    let spec = ops::GetAllInstruments::SPEC;
    let decode =
        |e: Encoding, body: &[u8]| -> m::ApiGetAllInstrumentsResponse { e.decode_response(spec, body).unwrap() };
    assert_eq!(decode(Encoding::Full, &full), decode(Encoding::Lite, &lite));
    println!(
        "\nget_all_instruments, 200 instruments: {} B full, {} B lite ({:+.0} %)",
        full.len(),
        lite.len(),
        (lite.len() as f64 / full.len() as f64 - 1.0) * 100.0
    );
    pair(
        "decode get_all_instruments (200)",
        500,
        || {
            black_box(decode(Encoding::Full, black_box(&full)));
        },
        || {
            black_box(decode(Encoding::Lite, black_box(&lite)));
        },
    );

    let req = order_request();
    let mut buf = Vec::with_capacity(1024);
    Encoding::Full
        .ws_request_into::<ops::CreateOrder>(&req, 7, &mut buf)
        .unwrap();
    let full_len = buf.len();
    Encoding::Lite
        .ws_request_into::<ops::CreateOrder>(&req, 7, &mut buf)
        .unwrap();
    println!(
        "\ncreate_order WS request: {full_len} B full, {} B lite ({:+.0} %)",
        buf.len(),
        (buf.len() as f64 / full_len as f64 - 1.0) * 100.0
    );
    let (mut full_buf, mut lite_buf) = (Vec::with_capacity(1024), Vec::with_capacity(1024));
    pair(
        "encode create_order WS request (kept buffer)",
        200_000,
        || {
            Encoding::Full
                .ws_request_into::<ops::CreateOrder>(black_box(&req), 7, &mut full_buf)
                .unwrap();
            black_box(&full_buf);
        },
        || {
            Encoding::Lite
                .ws_request_into::<ops::CreateOrder>(black_box(&req), 7, &mut lite_buf)
                .unwrap();
            black_box(&lite_buf);
        },
    );
}
