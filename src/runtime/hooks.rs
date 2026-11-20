//! User hooks.
//!
//! - `before_send(&mut Call)` runs before signing for every RPC (REST or WS). It may change
//!   `call.params`, but must not add, remove or reorder orders, or change their client order IDs (that is an `Error::Config` and
//!   nothing is sent). It runs before signing, so nothing can change a request after it is signed.
//! - `on_event(&Event)` is a read-only listener for connection, stream and retry events. It must not
//!   block: it runs on the socket's read task.
//!
//! An unset hook costs one `Option` check; the request is only turned into JSON for `before_send`
//! when that hook is set.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// How an RPC travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Rest,
    Ws,
}

/// One RPC call as `before_send` sees it.
#[derive(Debug, Clone)]
pub struct Call {
    /// e.g. "v1/create_order"
    pub rpc: &'static str,
    /// The request as JSON (full field names). Changes are decoded back into the request type.
    pub params: serde_json::Value,
    pub transport: Via,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
}

pub use super::protocol::GapRange;

/// What `on_event` receives. `socket` is "md" or "td".
#[derive(Debug, Clone)]
pub enum Event {
    Connect {
        socket: &'static str,
    },
    Reconnect {
        socket: &'static str,
    },
    Disconnect {
        socket: &'static str,
        code: Option<u16>,
        reason: String,
    },
    Subscribe {
        socket: &'static str,
        stream: String,
        selector: String,
        latest_sequence_number: u64,
        num_snapshots: u64,
    },
    Gap {
        gap: GapRange,
        socket: &'static str,
        stream: String,
        selector: String,
        /// Meaningful only for sequence_mismatch; zero on disconnect and overflow. Use gap for bounds.
        expected_prev: u64,
        prev: Option<u64>,
        seq: Option<u64>,
    },
    /// A read retried after `delay` (REST), or a socket reconnect attempt after a failed connect.
    Retry {
        socket: Option<&'static str>,
        rpc: Option<String>,
        attempt: u32,
        delay: Duration,
        error: String,
    },
    /// Raw bytes as sent or received. NOT redacted.
    Frame {
        socket: &'static str,
        direction: Direction,
        raw: Arc<[u8]>,
    },
    SessionRotated {
        websocket_reconnect_required: bool,
        generation: u64,
        expires_at: SystemTime,
    },
    Error {
        socket: Option<&'static str>,
        fatal: bool,
        error: String,
    },
}

/// The event's name, for filtering and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    Connect,
    Reconnect,
    Disconnect,
    Subscribe,
    Gap,
    Retry,
    Frame,
    SessionRotated,
    Error,
}

impl Event {
    pub fn kind(&self) -> EventKind {
        match self {
            Event::Connect { .. } => EventKind::Connect,
            Event::Reconnect { .. } => EventKind::Reconnect,
            Event::Disconnect { .. } => EventKind::Disconnect,
            Event::Subscribe { .. } => EventKind::Subscribe,
            Event::Gap { .. } => EventKind::Gap,
            Event::Retry { .. } => EventKind::Retry,
            Event::Frame { .. } => EventKind::Frame,
            Event::SessionRotated { .. } => EventKind::SessionRotated,
            Event::Error { .. } => EventKind::Error,
        }
    }
}

pub type BeforeSend = Arc<dyn Fn(&mut Call) + Send + Sync>;
pub type OnEvent = Arc<dyn Fn(&Event) + Send + Sync>;

#[derive(Clone, Default)]
pub struct Hooks {
    pub before_send: Option<BeforeSend>,
    pub on_event: Option<OnEvent>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks")
            .field("before_send", &self.before_send.is_some())
            .field("on_event", &self.on_event.is_some())
            .finish()
    }
}

impl Hooks {
    /// Calls `on_event` when set. Build the event inside `make` so nothing is allocated when unset.
    #[inline]
    pub fn emit(&self, make: impl FnOnce() -> Event) {
        if let Some(h) = &self.on_event {
            let event = make();
            // A listener must never break the runtime.
            if catch_unwind(AssertUnwindSafe(|| h(&event))).is_err() {
                tracing::error!(target: "grvt", "on_event hook panicked");
            }
        }
    }
}
