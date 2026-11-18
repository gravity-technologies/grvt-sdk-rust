use grvt::{models::WSOrderbookLevelsFeedDataV1, BookMode, OrderBook};
mod shared;
#[test]
fn shared_book_scenarios() {
    let cases = shared::load("scenarios/orderbook.json");
    for case in cases.as_array().unwrap() {
        let mut book = OrderBook::new(
            "X",
            if case["mode"] == "delta" {
                BookMode::Delta
            } else {
                BookMode::Snapshot
            },
        );
        for step in case["steps"].as_array().unwrap() {
            if step["action"] == "gap" {
                let reason = match step["reason"].as_str().unwrap() {
                    "disconnect" => "disconnect",
                    "overflow" => "overflow",
                    "sequence_mismatch" => "sequence_mismatch",
                    other => panic!("unknown gap {other}"),
                };
                book.on_gap(
                    "book.d",
                    "X@100",
                    &grvt::GapRange {
                        after_sequence: None,
                        through_sequence: None,
                        reason,
                    },
                );
                continue;
            }
            if step["action"] == "invalidate" {
                book.invalidate(None);
                continue;
            }
            let mut frame: WSOrderbookLevelsFeedDataV1 = serde_json::from_value(step["frame"].clone()).unwrap();
            let result = if step["action"] == "reset" {
                book.reset(&frame)
            } else {
                book.apply(&frame)
            };
            if let Some(reason) = step["error"].as_str() {
                let error = result.unwrap_err();
                assert_eq!(error.reason, reason, "{}", case["name"]);
                if reason == "gap" {
                    let gap = error.gap.unwrap();
                    assert_eq!(gap.after_sequence, Some(101));
                    assert_eq!(gap.through_sequence, None);
                }
                assert!(!book.ready());
                assert!(book.snapshot().is_err());
            } else {
                result.unwrap();
                let mut snapshot = book.snapshot().unwrap();
                assert_eq!(
                    serde_json::json!(snapshot.bids.iter().map(|v| v.price.to_string()).collect::<Vec<_>>()),
                    step["bids"],
                    "{}",
                    case["name"]
                );
                assert_eq!(
                    serde_json::json!(snapshot.asks.iter().map(|v| v.price.to_string()).collect::<Vec<_>>()),
                    step["asks"]
                );
                if let Some(level) = frame.feed.bids.as_mut().and_then(|v| v.first_mut()) {
                    level.num_orders = 999;
                }
                if let Some(level) = snapshot.bids.first_mut() {
                    level.num_orders = 888;
                    assert_eq!(book.snapshot().unwrap().bids[0].num_orders, 1);
                }
            }
        }
    }
}

#[cfg(feature = "client")]
#[test]
fn gap_event_invalidates_before_binding_and_filters_after_binding() {
    use grvt::{Event, GapRange};
    let mut book = OrderBook::new("X", BookMode::Delta);
    let event = |selector: &str| Event::Gap {
        socket: "md",
        stream: "book.d".into(),
        selector: selector.into(),
        gap: GapRange {
            after_sequence: Some(10),
            through_sequence: None,
            reason: "disconnect",
        },
        expected_prev: 10,
        prev: None,
        seq: None,
    };
    book.on_event(&event("X@100"));
    let frame = WSOrderbookLevelsFeedDataV1::default()
        .with_stream("book.d")
        .with_selector("X@100")
        .with_feed(grvt::models::OrderbookLevels::default().with_instrument("X"));
    assert_eq!(book.apply(&frame).unwrap_err().reason, "reset_required");
    book.reset(&frame).unwrap();
    book.on_event(&event("Y@100"));
    assert!(book.ready());
    book.on_event(&event("X@100"));
    assert!(!book.ready());
}
