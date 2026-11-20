//! Read-only, unauthenticated checks against Grvt testnet market data through the generated client.
//!
//! GRVT_SMOKE=1 cargo test --test smoke_testnet -- --ignored --nocapture
#![cfg(feature = "client")]

use std::future::IntoFuture;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use grvt::runtime::Gateway;
use grvt::{models as m, Config, Encoding, EventKind, Grvt};

#[tokio::test]
#[ignore = "hits testnet; run with GRVT_SMOKE=1 and --ignored"]
async fn testnet_market_data() {
    if std::env::var("GRVT_SMOKE").as_deref() != Ok("1") {
        eprintln!("skipped: set GRVT_SMOKE=1");
        return;
    }
    let events: Arc<Mutex<Vec<EventKind>>> = Arc::default();
    let ev = events.clone();
    // Full, because the raw WS request below sends full field names; lite is the default (and the next test).
    let client = Grvt::new(Config::testnet().encoding(Encoding::Full).on_event(move |e| {
        if e.kind() != EventKind::Frame {
            ev.lock().unwrap().push(e.kind());
        }
    }))
    .unwrap();

    let inst = client.get_instrument().instrument("BTC_USDT_Perp").await.unwrap();
    let r = &inst.result;
    assert_eq!(r.instrument_hash, "0x030501");
    assert_eq!(r.base_decimals, 9);

    let all = client.get_all_instruments().is_active(true).await.unwrap();
    let all = all.result.unwrap_or_default();
    assert!(all.iter().any(|i| i.instrument == "BTC_USDT_Perp"));
    let currencies = client.get_currency().await.unwrap().result.unwrap_or_default();
    assert!(currencies.iter().any(|c| c.symbol == "USDT"));

    // The WS JSON-RPC `result` is the same struct as the REST body.
    let raw = client
        .runtime()
        .socket(Gateway::MarketData)
        .request(
            "v1/instrument",
            br#"{"instrument":"BTC_USDT_Perp"}"#,
            false,
            None,
            Vec::new(),
        )
        .await
        .unwrap();
    let over_ws: m::ApiGetInstrumentResponse = serde_json::from_str(&raw).unwrap();
    assert_eq!(over_ws, inst);

    let mut ticks = client
        .ticker_delta_stream()
        .instrument("BTC_USDT_Perp")
        .rate(500)
        .await
        .unwrap();
    let mut got = Vec::new();
    while got.len() < 3 {
        let item = tokio::time::timeout(Duration::from_secs(20), ticks.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        got.push(item);
    }
    assert_eq!(got[0].sequence_number, 0);
    assert_eq!(got[0].feed.instrument.as_deref(), Some("BTC_USDT_Perp"));
    assert_eq!(got[2].prev_sequence_number, got[1].sequence_number);
    drop(ticks);

    let mut book = client
        .orderbook_snap_stream()
        .instrument("BTC_USDT_Perp")
        .rate(500)
        .depth(10)
        .await
        .unwrap();
    let snap = tokio::time::timeout(Duration::from_secs(20), book.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let bids = snap.feed.bids.clone().unwrap_or_default();
    assert!(!bids.is_empty());

    let kinds = events.lock().unwrap().clone();
    assert!(!kinds.contains(&EventKind::Gap), "{kinds:?}");
    println!(
        "\n  {} active instruments; ticker seqs {:?}; book best bid {:?}; events {:?}",
        all.len(),
        got.iter().map(|t| t.sequence_number).collect::<Vec<_>>(),
        bids[0].price,
        kinds
    );
    client.close().await;
}

/// Lite against testnet: the same calls and streams decode to the same models as full, and book frames
/// are smaller.
///
/// GRVT_SMOKE=1 cargo test --test smoke_testnet testnet_lite -- --ignored --nocapture
#[tokio::test]
#[ignore = "hits testnet; run with GRVT_SMOKE=1 and --ignored"]
async fn testnet_lite_matches_full() {
    if std::env::var("GRVT_SMOKE").as_deref() != Ok("1") {
        eprintln!("skipped: set GRVT_SMOKE=1");
        return;
    }
    type Sizes = Arc<Mutex<Vec<usize>>>;
    let connect = |encoding: Encoding| {
        let (sizes, events): (Sizes, Arc<Mutex<Vec<EventKind>>>) = Default::default();
        let (s, ev) = (sizes.clone(), events.clone());
        let client = Grvt::new(Config::testnet().encoding(encoding).on_event(move |e| match e {
            grvt::Event::Frame {
                direction: grvt::runtime::Direction::In,
                raw,
                ..
            } => s.lock().unwrap().push(raw.len()),
            e if e.kind() != EventKind::Frame => ev.lock().unwrap().push(e.kind()),
            _ => {}
        }))
        .unwrap();
        (client, sizes, events)
    };
    let (full, full_sizes, full_events) = connect(Encoding::Full);
    let (lite, lite_sizes, lite_events) = connect(Encoding::Lite);

    // HTTP
    let inst = full.get_instrument().instrument("BTC_USDT_Perp").await.unwrap();
    let b = lite.get_instrument().instrument("BTC_USDT_Perp").await.unwrap();
    assert_eq!(inst, b, "get_instrument");
    let a = full.get_all_instruments().is_active(true).await.unwrap();
    let b = lite.get_all_instruments().is_active(true).await.unwrap();
    assert_eq!(a, b, "get_all_instruments");
    let instruments = b.result.unwrap_or_default().len();
    assert!(instruments > 0);

    // WS RPC on the lite socket (request and double-wrapped result in lite)
    let raw = lite
        .runtime()
        .socket(Gateway::MarketData)
        .request("v1/instrument", br#"{"i":"BTC_USDT_Perp"}"#, false, None, Vec::new())
        .await
        .unwrap();
    let over_ws: m::ApiGetInstrumentResponse = Encoding::Lite.from_slice(raw.as_bytes()).unwrap();
    assert_eq!(over_ws, inst);

    // Order book snapshots, both subscribed at once. Testnet numbers each subscription's frames from its own
    // start (two subscriptions opened together differ by a constant offset), so the sequence numbers can't
    // pair them up. A book frame's event time can: the two frames for the same moment carry the same book,
    // and each client's own frames must follow on without a gap.
    fn subscribe(client: &Grvt) -> grvt::runtime::StreamCall<'_, m::WSOrderbookLevelsFeedDataV1, impl Sized> {
        client
            .orderbook_snap_stream()
            .instrument("BTC_USDT_Perp")
            .rate(500)
            .depth(10)
    }
    let (a, b) = tokio::join!(subscribe(&full).into_future(), subscribe(&lite).into_future());
    let read = |mut book: grvt::Subscription<m::WSOrderbookLevelsFeedDataV1>| async move {
        let mut got = Vec::new();
        while got.len() < 12 {
            let f = tokio::time::timeout(Duration::from_secs(20), book.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(f.stream, "v1.book.s");
            assert_eq!(f.feed.instrument, "BTC_USDT_Perp");
            assert!(
                !f.feed.bids.clone().unwrap_or_default().is_empty(),
                "frame not decoded: {f:?}"
            );
            got.push(f);
        }
        got
    };
    let (a, b) = tokio::join!(read(a.unwrap()), read(b.unwrap()));
    let same: Vec<_> = a
        .iter()
        .filter_map(|f| {
            b.iter()
                .find(|l| l.feed.event_time == f.feed.event_time)
                .map(|l| (f, l))
        })
        .collect();
    assert!(!same.is_empty(), "no book snapshot seen by both clients");
    for (f, l) in &same {
        assert_eq!(f.feed, l.feed, "book at {:?}", f.feed.event_time);
    }
    assert!(
        !full_events.lock().unwrap().contains(&EventKind::Gap),
        "gap on the full book stream"
    );
    assert!(
        !lite_events.lock().unwrap().contains(&EventKind::Gap),
        "gap on the lite book stream"
    );

    // Ticker deltas (frames carry keys the contract does not list)
    let mut ticks = lite
        .ticker_delta_stream()
        .instrument("BTC_USDT_Perp")
        .rate(500)
        .await
        .unwrap();
    let mut seqs = Vec::new();
    while seqs.len() < 3 {
        let t = tokio::time::timeout(Duration::from_secs(20), ticks.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(t.feed.instrument.as_deref(), Some("BTC_USDT_Perp"));
        seqs.push((t.sequence_number, t.prev_sequence_number));
    }
    assert_eq!(seqs[2].1, seqs[1].0);
    drop(ticks);
    assert!(!lite_events.lock().unwrap().contains(&EventKind::Gap));

    let avg = |s: &Sizes| {
        let s = s.lock().unwrap();
        let big: Vec<_> = s.iter().filter(|n| **n > 400).collect();
        big.iter().copied().sum::<usize>() / big.len().max(1)
    };
    println!(
        "\n  {instruments} instruments equal; {} book snapshots with the same event time equal; \
         avg book-sized frame {} B full vs {} B lite",
        same.len(),
        avg(&full_sizes),
        avg(&lite_sizes)
    );
    full.close().await;
    lite.close().await;
}
