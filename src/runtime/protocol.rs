//! WebSocket protocol state as plain structs: no sockets, no timers, no tasks. The client uses them, and
//! an application with its own event loop can too: one `RequestTable` per connection, one `Sequencer` per
//! subscription, one `Backoff` per reconnect loop.
//!
//! - `RequestTable`: JSON-RPC uint32 id -> pending request; `connection_lost` fails every pending request
//! - `Sequencer`: per-subscription sequence tracking and gap detection
//! - `Backoff`: full-jitter reconnect delays
//!
//! Server behaviour this follows:
//! - The subscribe reply comes first, then snapshot frames (sequence_number 0), then buffered deltas
//!   newer than `latest_sequence_number`, then live deltas, all in order on one connection.
//! - Every delta carries `prev_sequence_number`; with global sequence numbers the next delta's prev
//!   equals the previous delta's number, so a mismatch is a gap.
//! - Subscribing again to the same primary selector replaces the old one and sends a fresh snapshot.
//! - Seen on testnet: after a re-subscribe on the same socket, the first delta's prev_sequence_number
//!   is the OLD last sequence number, not the reply's `latest_sequence_number`. So the first sequenced
//!   frame after a (re)subscribe reply is accepted when its prev is 0 or not greater than the latest
//!   sequence number known then; after that, prev must equal the last sequence number seen, otherwise
//!   it is a gap.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

use super::error::Error;
use crate::generated::behavior;

/// Potentially missed sequences: exclusive start, inclusive end. None means unknown.
/// An open end includes re-subscription. REST history cannot use these as resume cursors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapRange {
    pub after_sequence: Option<u64>,
    pub through_sequence: Option<u64>,
    /// "sequence_mismatch", "disconnect" or "overflow".
    pub reason: &'static str,
}

/// One request waiting for its reply. `waiter` is whatever the socket needs to resolve it.
#[derive(Debug)]
pub struct Pending<W> {
    pub method: String,
    pub write: bool,
    /// The client order ids a write carries (from `prepare_request`), reported if its outcome is unknown.
    pub client_order_ids: Vec<u64>,
    pub waiter: W,
}

impl<W> Pending<W> {
    /// A request that carries no orders.
    pub fn new(method: impl Into<String>, write: bool, waiter: W) -> Self {
        Pending {
            method: method.into(),
            write,
            client_order_ids: Vec::new(),
            waiter,
        }
    }
}

/// A pending request that will get no reply, with the error to resolve it with.
#[derive(Debug)]
pub struct Failed<W> {
    pub id: u32,
    pub request: Pending<W>,
    pub error: Error,
}

/// uint32 ids; 0 is never used (the gateway omits id 0 in replies).
#[derive(Debug)]
pub struct RequestTable<W> {
    next: u32,
    pending: HashMap<u32, Pending<W>>,
}

impl<W> Default for RequestTable<W> {
    fn default() -> Self {
        Self::starting_at(1)
    }
}

impl<W> RequestTable<W> {
    pub fn starting_at(start: u32) -> Self {
        Self {
            next: start.max(1),
            pending: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Store `entry` under a fresh id. `None` only when all 2^32-1 ids are in use.
    pub fn add(&mut self, entry: Pending<W>) -> Option<u32> {
        for _ in 0..u32::MAX {
            let id = self.next;
            self.next = if self.next == u32::MAX { 1 } else { self.next + 1 };
            if let std::collections::hash_map::Entry::Vacant(v) = self.pending.entry(id) {
                v.insert(entry);
                return Some(id);
            }
        }
        None
    }

    pub fn pop(&mut self, id: u32) -> Option<Pending<W>> {
        self.pending.remove(&id)
    }

    pub fn writes_in_flight(&self) -> usize {
        self.pending.values().filter(|p| p.write).count()
    }

    /// The connection dropped: every pending request fails. A write may have reached the exchange, so it
    /// becomes `OrderStateUnknown` (never resend it blindly); a read is a transport error (sent) and may be
    /// retried on the next connection.
    pub fn connection_lost(&mut self, reason: &str) -> Vec<Failed<W>> {
        self.pending
            .drain()
            .map(|(id, p)| Self::fail(id, p, reason, true))
            .collect()
    }

    /// No reply in time: the same rule for one request.
    pub fn timed_out(&mut self, id: u32, reason: &str) -> Option<Failed<W>> {
        let p = self.pop(id)?;
        Some(Self::fail(id, p, reason, true))
    }

    /// The frame was never written (the socket closed first): the request provably did not leave the
    /// process, so even a write is a plain transport error.
    pub fn not_sent(&mut self, id: u32, reason: &str) -> Option<Failed<W>> {
        let p = self.pop(id)?;
        Some(Self::fail(id, p, reason, false))
    }

    fn fail(id: u32, request: Pending<W>, reason: &str, sent: bool) -> Failed<W> {
        let error = if request.write && sent {
            let mut error = Error::order_state_unknown(&request.method, reason, request.client_order_ids.clone());
            if let Error::OrderStateUnknown { source, .. } = &mut error {
                *source = Some(Box::new(Error::transport(reason, Some(&request.method), sent)));
            }
            error
        } else {
            Error::transport(reason, Some(&request.method), sent)
        };
        Failed { id, request, error }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Deliver,
    /// Duplicate, or a frame from before the current subscribe reply.
    Drop,
    Gap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SeqState {
    #[default]
    AwaitingAck,
    Live,
}

#[derive(Debug, Clone, Default)]
pub struct Sequencer {
    pub state: SeqState,
    pub last: u64,
    pub snapshots_left: u64,
    pub gaps: u64,
    /// Set by the caller when it re-subscribes after a gap; cleared when the snapshot arrives.
    pub resync_started: Option<Instant>,
    /// The first sequenced frame after a (re)subscribe reply is still to come.
    first: bool,
}

impl Sequencer {
    pub fn reset(&mut self) {
        self.state = SeqState::AwaitingAck;
        self.last = 0;
        self.snapshots_left = 0;
    }

    pub fn on_ack(&mut self, latest_sequence_number: u64, num_snapshots: u64) {
        self.state = SeqState::Live;
        self.last = latest_sequence_number;
        self.snapshots_left = num_snapshots;
        self.first = true;
        if num_snapshots == 0 {
            self.resync_started = None;
        }
    }

    pub fn on_frame(&mut self, seq: Option<u64>, prev: Option<u64>) -> Verdict {
        if self.state == SeqState::AwaitingAck {
            return Verdict::Drop;
        }
        let seq = match seq {
            None | Some(0) => {
                // snapshot (sequence_number 0) or an unsequenced frame
                if self.snapshots_left > 0 {
                    self.snapshots_left -= 1;
                    if self.snapshots_left == 0 {
                        self.resync_started = None;
                    }
                }
                return Verdict::Deliver;
            }
            Some(s) => s,
        };
        if seq <= self.last {
            return Verdict::Drop;
        }
        let first = std::mem::replace(&mut self.first, false);
        if let Some(p) = prev {
            // The re-subscribe quirk above: self.last is still the reply's latest_sequence_number here.
            if p != self.last && !(first && p <= self.last) {
                self.gaps += 1;
                return Verdict::Gap;
            }
        }
        self.last = seq;
        Verdict::Deliver
    }

    pub fn resync_overdue(&self, now: Instant, limit: Duration) -> bool {
        self.resync_started.is_some_and(|t| now.duration_since(t) > limit)
    }
}

/// Full jitter: sleep uniform(0, min(cap, base * 2^attempt)).
#[derive(Debug)]
pub struct Backoff {
    pub base: Duration,
    pub cap: Duration,
    pub attempt: u32,
    rng: StdRng,
}

impl Backoff {
    pub fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            attempt: 0,
            rng: rand::make_rng(),
        }
    }

    pub fn seeded(base: Duration, cap: Duration, seed: u64) -> Self {
        Self {
            base,
            cap,
            attempt: 0,
            rng: StdRng::seed_from_u64(seed),
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let ceiling = self.base.saturating_mul(1u32 << self.attempt.min(30)).min(self.cap);
        self.attempt = (self.attempt + 1).min(30);
        ceiling.mul_f64(self.rng.random::<f64>())
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(behavior::RECONNECT_BACKOFF_BASE, behavior::RECONNECT_BACKOFF_CAP)
    }
}

/// Build the request text without re-encoding already-encoded params (full names; lite:
/// `Encoding::Lite.jsonrpc_frame`).
pub fn jsonrpc_frame(method: &str, params_json: &[u8], id: u32) -> String {
    super::lite::Encoding::Full.jsonrpc_frame(method, params_json, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_skip_zero_and_in_use() {
        let mut t = RequestTable::starting_at(u32::MAX);
        let a = t.add(Pending::new("a", false, ())).unwrap();
        let b = t.add(Pending::new("b", true, ())).unwrap();
        assert_eq!((a, b), (u32::MAX, 1));
        assert_eq!(t.writes_in_flight(), 1);
        assert_eq!(t.pop(a).unwrap().method, "a");
        assert!(t.pop(a).is_none());
        assert_eq!(t.connection_lost("closed").len(), 1);
        assert!(t.is_empty());
    }

    #[test]
    fn request_ids_wrap_around_pending_entries() {
        let mut t = RequestTable::starting_at(1);
        t.add(Pending::new("1", false, ()));
        t.next = 1;
        assert_eq!(t.add(Pending::new("2", false, ())), Some(2));
    }

    #[test]
    fn lost_connection_fails_writes_as_unknown() {
        let mut t = RequestTable::default();
        let mut write = Pending::new("v1/create_order", true, ());
        write.client_order_ids = vec![7];
        let w = t.add(write).unwrap();
        let r = t.add(Pending::new("v1/instrument", false, ())).unwrap();
        let unsent = t.add(Pending::new("v1/create_order", true, ())).unwrap();
        let not_sent = t.not_sent(unsent, "closed before send").unwrap();
        assert!(matches!(not_sent.error, Error::Transport { sent: false, .. }));
        let timed_out = t.timed_out(r, "no reply").unwrap();
        assert!(matches!(timed_out.error, Error::Transport { sent: true, .. }));
        let lost = t.connection_lost("socket closed");
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].id, w);
        assert!(matches!(
            &lost[0].error,
            Error::OrderStateUnknown {
                client_order_id: Some(7),
                ..
            }
        ));
        assert!(t.is_empty());
    }

    #[test]
    fn backoff_is_full_jitter_and_capped() {
        let mut b = Backoff::seeded(Duration::from_millis(100), Duration::from_secs(1), 7);
        for attempt in 0..10u32 {
            let d = b.next_delay();
            let ceiling = Duration::from_millis(100)
                .saturating_mul(1 << attempt)
                .min(Duration::from_secs(1));
            assert!(d <= ceiling, "attempt {attempt}: {d:?} > {ceiling:?}");
        }
        b.reset();
        assert_eq!(b.attempt, 0);
    }

    #[test]
    fn frame_embeds_params_verbatim() {
        assert_eq!(
            jsonrpc_frame("v1/instrument", br#"{"instrument":"X"}"#, 7),
            r#"{"jsonrpc":"2.0","method":"v1/instrument","params":{"instrument":"X"},"id":7}"#
        );
    }
}
