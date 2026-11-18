//! Hot-path timings (no criterion; plain loops, median of 7 runs). `cargo bench --bench hot_path`.
//!
//! - sign one 2-leg OrderInput: core `sign_order_create_order` (metadata passed in), and the client's
//!   signing (metadata cached)
//! - OrderInput hash and hash+sign: generated encoder vs the reference (typed message + generic encoder),
//!   with heap allocations per op (counting allocator)
//! - the parts: secp256k1 (libsecp256k1) vs k256, tiny-keccak vs sha3
//! - encode a signed create_order request (fresh `Vec` and kept-buffer `_into` forms), decode a 10-level
//!   book delta
//! - the client's stream path for one 10+10 book frame: parse once, route, sequence, decode the payload
//!
//! Every op also prints its heap allocations and bytes (counting allocator). The instrument carries real
//! limits, so the order checks run in full.

#[path = "../tests/reference/mod.rs"]
mod reference;

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use std::collections::HashMap;

use grvt::runtime::clock::FixedClock;
use grvt::runtime::signing::keccak256;
use grvt::runtime::{InstrumentMeta, Meta};
use grvt::{enums, models as m, ops, signing, Config, Grvt, PrivateKeySigner, SignContext, Signer, Timestamp};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(l.size(), Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Heap allocations and bytes of one call.
fn allocs(f: impl FnOnce()) -> String {
    let (a, b) = (ALLOCS.load(Relaxed), BYTES.load(Relaxed));
    f();
    format!("{} allocs, {} B", ALLOCS.load(Relaxed) - a, BYTES.load(Relaxed) - b)
}

const KEY: &str = "0xbfe25ff07bd38cb3fbc28a2a296d41aabd68a78c929d257935906489fc9c6d89";

fn bench(name: &str, iters: u32, mut f: impl FnMut()) {
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
        println!("{name:<46} {:>9.2} µs/op", ns / 1000.0);
    } else {
        println!("{name:<46} {:>9.0} ns/op", ns);
    }
}

fn order_request() -> m::ApiCreateOrderRequestInput {
    m::ApiCreateOrderRequestInput::default().with_order(
        m::OrderInput::default()
            .with_sub_account_id(8289849667772468)
            .with_time_in_force(enums::TimeInForce::GoodTillTime)
            .with_post_only(true)
            .with_legs(vec![
                m::OrderLeg::default()
                    .with_instrument("BTC_USDT_Perp")
                    .with_size(rust_decimal::Decimal::from_str_exact("1.5").unwrap())
                    .with_limit_price(rust_decimal::Decimal::from_str_exact("65000.5").unwrap())
                    .with_is_buying_asset(true),
                m::OrderLeg::default()
                    .with_instrument("BTC_USDT_Perp")
                    .with_size(rust_decimal::Decimal::from_str_exact("0.25").unwrap())
                    .with_limit_price(rust_decimal::Decimal::from_str_exact("64000").unwrap())
                    .with_is_buying_asset(false),
            ])
            .with_metadata(m::OrderMetadataInput::default().with_client_order_id(1001))
            .with_signature(
                m::SignatureInput::default()
                    .with_nonce(7)
                    .with_expiration(grvt::Timestamp::from_nanoseconds(1_700_000_000_000_000_000)),
            ),
    )
}

fn book_delta() -> Vec<u8> {
    let level = |p: f64| serde_json::json!({"price": format!("{p:.1}"), "size": "1.234", "num_orders": 3});
    let bids: Vec<_> = (0..10).map(|i| level(65000.0 - i as f64)).collect();
    let asks: Vec<_> = (0..10).map(|i| level(65001.0 + i as f64)).collect();
    serde_json::to_vec(&serde_json::json!({
        "stream": "v1.book.d", "selector": "BTC_USDT_Perp@100-10", "sequence_number": "8082025237400000",
        "prev_sequence_number": "8082025237300000",
        "feed": {"event_time": "1700000000000000000", "instrument": "BTC_USDT_Perp", "bids": bids, "asks": asks},
    }))
    .unwrap()
}

fn print_allocs(f: impl FnOnce()) {
    println!("{:<46} {:>12}", "", allocs(f));
}

fn btc_meta() -> InstrumentMeta {
    InstrumentMeta {
        instrument_hash: "0x030501".into(),
        base_decimals: 9,
        limits: [("tick_size", "0.1"), ("min_size", "0.001"), ("min_notional", "10")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        ..Default::default()
    }
}

fn main() {
    let client = Grvt::new(
        Config::testnet()
            .signer(PrivateKeySigner::new(KEY).unwrap())
            .clock(FixedClock(1_700_000_000_000_000_000)),
    )
    .unwrap();
    client.runtime().metadata().put_instrument("BTC_USDT_Perp", btc_meta());
    let rt = client.runtime();
    let req = order_request();

    bench("sign OrderInput (client signing, end to end)", 20_000, || {
        let signed = futures::executor::block_on(client.sign_order_create_order(black_box(req.clone()))).unwrap();
        black_box(signed);
    });
    let r = req.clone();
    print_allocs(|| {
        black_box(futures::executor::block_on(client.sign_order_create_order(r)).unwrap());
    });

    let signer = PrivateKeySigner::new(KEY).unwrap();
    let mut meta = Meta::default();
    meta.instruments.insert("BTC_USDT_Perp".into(), btc_meta());
    let mut ctx = SignContext::new(&signer, &meta, 326);
    ctx.now = Some(Timestamp::from_nanoseconds(1_700_000_000_000_000_000));
    bench("sign OrderInput (core sign_order_create_order)", 20_000, || {
        let mut r = black_box(req.clone());
        signing::sign_order_create_order(&mut r, &ctx).unwrap();
        black_box(r);
    });
    let mut r = req.clone();
    print_allocs(|| signing::sign_order_create_order(&mut r, &ctx).unwrap());
    let reference = || {
        let msg = signing::message_order(&req, &meta).unwrap();
        reference::digest(&signing::EIP712_ORDER, rt.domain(), &msg).unwrap()
    };
    let generated = || grvt::runtime::signing::digest(326, &signing::hash_order(&req, &meta).unwrap());
    assert_eq!(reference(), generated());
    for (name, f) in [
        (
            "OrderInput 2 legs: hash, reference (before)",
            &reference as &dyn Fn() -> [u8; 32],
        ),
        ("OrderInput 2 legs: hash, generated (after)", &generated),
    ] {
        bench(name, 100_000, || {
            black_box(f());
        });
        println!(
            "{:<46} {:>12}",
            "",
            allocs(|| {
                black_box(f());
            })
        );
    }
    for (name, f) in [
        (
            "OrderInput 2 legs: hash+sign, reference (before)",
            &reference as &dyn Fn() -> [u8; 32],
        ),
        ("OrderInput 2 legs: hash+sign, generated (after)", &generated),
    ] {
        let sign = || signer.sign_prehash(&f()).unwrap().unwrap();
        bench(name, 20_000, || {
            black_box(sign());
        });
        println!(
            "{:<46} {:>12}",
            "",
            allocs(|| {
                black_box(sign());
            })
        );
    }

    let digest = keccak256(b"digest");
    bench("  secp256k1 (libsecp256k1) recoverable sign", 20_000, || {
        black_box(signer.sign_digest(black_box(&digest)));
    });
    let k = k256::ecdsa::SigningKey::from_slice(&hex(KEY)).unwrap();
    bench("  k256 recoverable sign", 20_000, || {
        black_box(k.sign_prehash_recoverable(black_box(&digest)));
    });
    let data = vec![7u8; 9 * 32];
    bench("  keccak256 288 B (sha3, used)", 200_000, || {
        black_box(keccak256(black_box(&data)));
    });
    bench("  keccak256 288 B (tiny-keccak)", 200_000, || {
        use tiny_keccak::{Hasher, Keccak};
        let mut k = Keccak::v256();
        k.update(black_box(&data));
        let mut out = [0u8; 32];
        k.finalize(&mut out);
        black_box(out);
    });

    let mut signed = req.clone();
    signing::sign_order_create_order(&mut signed, &ctx).unwrap();
    bench("encode create_order request (serde_json)", 200_000, || {
        black_box(serde_json::to_vec(black_box(&signed)).unwrap());
    });
    print_allocs(|| {
        black_box(serde_json::to_vec(&signed).unwrap());
    });
    let mut buf = Vec::new();
    bench("encode create_order, kept buffer (_into)", 200_000, || {
        grvt::encode_request_into(black_box(&signed), &mut buf).unwrap();
        black_box(&buf);
    });
    print_allocs(|| grvt::encode_request_into(&signed, &mut buf).unwrap());
    bench("encode create_order WS frame (ws_request)", 200_000, || {
        black_box(grvt::ws_request::<ops::CreateOrder>(black_box(&signed), 7).unwrap());
    });
    print_allocs(|| {
        black_box(grvt::ws_request::<ops::CreateOrder>(&signed, 7).unwrap());
    });
    bench("encode create_order WS frame, kept buffer", 200_000, || {
        grvt::ws_request_into::<ops::CreateOrder>(black_box(&signed), 7, &mut buf).unwrap();
        black_box(&buf);
    });
    print_allocs(|| grvt::ws_request_into::<ops::CreateOrder>(&signed, 7, &mut buf).unwrap());
    let raw = book_delta();
    bench("decode book delta, 10+10 levels (serde_json)", 100_000, || {
        black_box(serde_json::from_slice::<m::WSOrderbookLevelsFeedDataV1>(black_box(&raw)).unwrap());
    });
    print_allocs(|| {
        black_box(serde_json::from_slice::<m::WSOrderbookLevelsFeedDataV1>(&raw).unwrap());
    });

    // What the client does with one book frame: the read loop parses it once and routes it by stream and
    // selector through the sequencer; the consumer decodes only the payload.
    let mut subs: HashMap<String, HashMap<String, usize>> = HashMap::new();
    subs.entry("v1.book.d".into())
        .or_default()
        .insert("BTC_USDT_Perp@100-10".into(), 0);
    let mut seq = grvt::Sequencer::default();
    let mut stream_path = || {
        seq.on_ack(8082025237300000, 0);
        let msg = grvt::parse_ws_message(black_box(&raw)).unwrap();
        let routed = subs
            .get(msg.stream.as_deref().unwrap())
            .and_then(|m| m.get(msg.selector.as_deref().unwrap()));
        black_box(routed);
        assert_eq!(
            seq.on_frame(msg.sequence_number, msg.prev_sequence_number),
            grvt::Verdict::Deliver
        );
        black_box(grvt::decode_feed::<ops::OrderbookDeltaStream>(&msg).unwrap());
    };
    bench(
        "stream path: parse once, route, decode payload",
        100_000,
        &mut stream_path,
    );
    print_allocs(stream_path);
}

fn hex(s: &str) -> Vec<u8> {
    let h = s.trim_start_matches("0x");
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
        .collect()
}
