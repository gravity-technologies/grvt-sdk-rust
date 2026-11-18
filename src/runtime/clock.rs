//! Time, nonces and client order ids. The client takes all three as options and the core sign context
//! takes the first two, so tests can pin them.

use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::generated::behavior;

pub trait Clock: Send + Sync {
    /// Nanoseconds since the Unix epoch.
    fn now_ns(&self) -> i64;
}

pub trait NonceProvider: Send + Sync {
    /// uint32 signature nonce (the EIP-712 Order.nonce is uint32).
    fn next_nonce(&self) -> u32;

    /// What signing calls: a source that can fail (the OS random source) overrides it and returns
    /// `Error::Signing`, so signing stops instead of panicking.
    fn try_next_nonce(&self) -> crate::Result<u32> {
        Ok(self.next_nonce())
    }
}

pub trait ClientOrderIdProvider: Send + Sync {
    fn next_id(&self) -> u64;
}

fn wall_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Local wall clock plus an optional offset to the server clock. `observe(server_ms, rtt_ms)` (e.g.
/// from GET /time) moves the offset toward the server with exponential smoothing.
#[derive(Debug, Default)]
pub struct SystemClock {
    offset_ns: AtomicI64,
    samples: Mutex<u32>,
}

impl SystemClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&self, server_ms: i64, rtt_ms: f64) {
        let local_mid = wall_ns() - (rtt_ms * 1e6 / 2.0) as i64;
        let sample = server_ms * 1_000_000 - local_mid;
        let mut n = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        let next = if *n == 0 {
            sample
        } else {
            (self.offset_ns.load(Ordering::Relaxed) as f64 * 0.8 + sample as f64 * 0.2) as i64
        };
        self.offset_ns.store(next, Ordering::Relaxed);
        *n += 1;
    }

    pub fn offset_ns(&self) -> i64 {
        self.offset_ns.load(Ordering::Relaxed)
    }
}

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        wall_ns() + self.offset_ns.load(Ordering::Relaxed)
    }
}

/// Always returns the same time.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub i64);

impl Clock for FixedClock {
    fn now_ns(&self) -> i64 {
        self.0
    }
}

/// Signature nonces straight from the operating system's random source (`getrandom`), not from a seeded
/// generator.
#[derive(Debug, Default, Clone, Copy)]
pub struct RandomNonce;

impl NonceProvider for RandomNonce {
    /// Panics if the OS random source fails; signing uses `try_next_nonce`, which returns an error.
    fn next_nonce(&self) -> u32 {
        self.try_next_nonce()
            .expect("the operating system's random source failed")
    }

    fn try_next_nonce(&self) -> crate::Result<u32> {
        use rand::TryRng;
        rand::rngs::SysRng
            .try_next_u32()
            .map_err(|e| crate::Error::Signing(format!("no signature nonce: the OS random source failed: {e}")))
    }
}

/// `1 << 63 | seconds << 31 | counter`: unique per process, sortable, and in the API client range
/// [2^63, 2^64). The counter starts at a random 31-bit value, so separate processes rarely collide.
#[derive(Debug)]
pub struct TimeCounterClientOrderId {
    counter: AtomicU32,
}

impl Default for TimeCounterClientOrderId {
    fn default() -> Self {
        Self::starting_at(rand::random::<u32>())
    }
}

impl TimeCounterClientOrderId {
    /// Only the low 31 bits of `start` are used.
    fn starting_at(start: u32) -> Self {
        Self {
            counter: AtomicU32::new(start),
        }
    }
}

impl ClientOrderIdProvider for TimeCounterClientOrderId {
    fn next_id(&self) -> u64 {
        let seconds = (wall_ns() / 1_000_000_000) as u64 & behavior::CLIENT_ORDER_ID_SECONDS_MASK;
        let counter = u64::from(self.counter.fetch_add(1, Ordering::Relaxed)) & behavior::CLIENT_ORDER_ID_COUNTER_MASK;
        (1 << 63) | (seconds << behavior::CLIENT_ORDER_ID_COUNTER_BITS) | counter
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_order_id_counter_wraps_inside_the_api_client_range() {
        for start in [0x7FFF_FFFF, u32::MAX] {
            let ids = TimeCounterClientOrderId::starting_at(start);
            let (a, b) = (ids.next_id(), ids.next_id());
            assert!(a >= 1 << 63 && b >= 1 << 63 && a != b, "{a} {b}");
            assert_eq!(b & 0x7FFF_FFFF, (a + 1) & 0x7FFF_FFFF);
        }
    }

    #[test]
    fn client_order_id_counter_starts_anywhere_in_31_bits() {
        let ids = TimeCounterClientOrderId::starting_at(0x7FFF_FFFF);
        assert_eq!(ids.next_id() & 0x7FFF_FFFF, 0x7FFF_FFFF);
    }
}
