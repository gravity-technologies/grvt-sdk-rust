//! Local price-level books for one subscription. No I/O: feed it frames (`reset`, `apply`) and gaps
//! (`on_gap`, or the client's `on_event`).
use crate::runtime::protocol::GapRange;
use crate::{
    models::{OrderbookLevel, WSOrderbookLevelsFeedDataV1},
    Timestamp,
};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookMode {
    Delta,
    Snapshot,
}
#[derive(Debug, Clone, thiserror::Error)]
#[error("order book: {reason}; reset from a fresh subscription snapshot")]
pub struct BookError {
    pub reason: &'static str,
    pub gap: Option<GapRange>,
}
#[derive(Debug, Clone)]
pub struct BookSnapshot {
    pub instrument: String,
    pub sequence_number: u64,
    pub event_time: Timestamp,
    pub bids: Vec<OrderbookLevel>,
    pub asks: Vec<OrderbookLevel>,
}

/// Accepts frames from the SDK, which validates the initial sequence against its
/// subscribe acknowledgment. Delta mode needs a fresh subscription snapshot after a gap.
/// Snapshot mode accepts complete frames after reconnect, including sequence zero.
pub struct OrderBook {
    instrument: String,
    mode: BookMode,
    ready: bool,
    invalidated: bool,
    gap: Option<GapRange>,
    scope: Option<(String, String)>,
    sequence: u64,
    time: Timestamp,
    bids: BTreeMap<Decimal, OrderbookLevel>,
    asks: BTreeMap<Decimal, OrderbookLevel>,
}
impl OrderBook {
    pub fn new(instrument: impl Into<String>, mode: BookMode) -> Self {
        Self {
            instrument: instrument.into(),
            mode,
            ready: false,
            invalidated: false,
            gap: None,
            scope: None,
            sequence: 0,
            time: Timestamp::default(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
        }
    }
    pub fn ready(&self) -> bool {
        self.ready
    }
    pub fn invalidate(&mut self, gap: Option<GapRange>) {
        self.ready = false;
        self.invalidated = true;
        self.gap = gap;
        self.bids.clear();
        self.asks.clear();
    }
    /// A gap on `stream` / `selector`: a delta book invalidates itself if the gap is on its subscription
    /// (or it has seen no frame yet) and needs a fresh snapshot.
    pub fn on_gap(&mut self, stream: &str, selector: &str, gap: &GapRange) {
        if self.mode == BookMode::Snapshot {
            return;
        }
        if self.scope.is_none() || self.scope.as_ref().is_some_and(|(s, k)| s == stream && k == selector) {
            self.invalidate(Some(gap.clone()));
        }
    }

    /// Forward the client's events: a `Gap` goes to `on_gap`, everything else is ignored.
    #[cfg(feature = "client")]
    pub fn on_event(&mut self, event: &crate::Event) {
        if let crate::Event::Gap {
            stream, selector, gap, ..
        } = event
        {
            self.on_gap(stream, selector, gap);
        }
    }
    fn fail(&mut self, reason: &'static str, gap: Option<GapRange>) -> BookError {
        self.invalidate(gap.clone());
        BookError { reason, gap }
    }
    pub fn reset(&mut self, frame: &WSOrderbookLevelsFeedDataV1) -> Result<(), BookError> {
        if self.mode == BookMode::Delta && frame.sequence_number != 0 {
            return Err(self.fail("snapshot_required", None));
        }
        self.invalidated = false;
        self.ready = false;
        self.apply(frame)
    }
    pub fn apply(&mut self, frame: &WSOrderbookLevelsFeedDataV1) -> Result<(), BookError> {
        if self.invalidated {
            return Err(BookError {
                reason: "reset_required",
                gap: self.gap.clone(),
            });
        }
        let scope = (frame.stream.clone(), frame.selector.clone());
        if frame.feed.instrument != self.instrument || self.scope.as_ref().is_some_and(|s| s != &scope) {
            return Err(self.fail("wrong_stream", None));
        }
        if self.ready
            && !(self.mode == BookMode::Snapshot && frame.sequence_number == 0)
            && frame.sequence_number <= self.sequence
        {
            return Err(self.fail("out_of_order", None));
        }
        let snapshot = self.mode == BookMode::Snapshot || frame.sequence_number == 0;
        if !snapshot {
            if !self.ready {
                return Err(BookError {
                    reason: "not_ready",
                    gap: self.gap.clone(),
                });
            }
            if self.sequence != 0 && frame.prev_sequence_number != self.sequence {
                return Err(self.fail(
                    "gap",
                    Some(GapRange {
                        after_sequence: Some(self.sequence),
                        through_sequence: None,
                        reason: "sequence_mismatch",
                    }),
                ));
            }
            if frame.sequence_number <= self.sequence || frame.sequence_number <= frame.prev_sequence_number {
                return Err(self.fail("out_of_order", None));
            }
        }
        let bids = frame.feed.bids.as_deref().unwrap_or_default();
        let asks = frame.feed.asks.as_deref().unwrap_or_default();
        for level in bids.iter().chain(asks) {
            if level.price <= Decimal::ZERO || level.size < Decimal::ZERO {
                return Err(self.fail("invalid_level", None));
            }
        }
        if snapshot {
            self.bids.clear();
            self.asks.clear();
        }
        for (levels, side) in [(bids, &mut self.bids), (asks, &mut self.asks)] {
            for level in levels {
                if level.size.is_zero() {
                    side.remove(&level.price);
                } else {
                    side.insert(level.price, level.clone());
                }
            }
        }
        self.scope = Some(scope);
        self.sequence = frame.sequence_number;
        self.time = frame.feed.event_time;
        self.ready = true;
        self.gap = None;
        Ok(())
    }
    pub fn snapshot(&self) -> Result<BookSnapshot, BookError> {
        if !self.ready {
            return Err(BookError {
                reason: "not_ready",
                gap: self.gap.clone(),
            });
        }
        Ok(BookSnapshot {
            instrument: self.instrument.clone(),
            sequence_number: self.sequence,
            event_time: self.time,
            bids: self.bids.values().rev().cloned().collect(),
            asks: self.asks.values().cloned().collect(),
        })
    }
}
