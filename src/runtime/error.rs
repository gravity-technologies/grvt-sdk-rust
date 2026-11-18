//! Errors returned by the SDK.
//!
//! - `Api`: the server answered with an error code.
//! - `RateLimit`: the server said "slow down" (code 1006).
//! - `Transport`: we could not get an answer (connect failure, timeout on a read, socket closed).
//! - `OrderStateUnknown`: a write may or may not have reached the exchange. Never resend blindly:
//!   look the order up by `client_order_id` first.
//! - `StreamOverflow`: a consumer fell behind a stream that may not drop items.
//! - `OrderOutOfBounds`: an order leg breaks its instrument's tick, size step, minimum size or minimum
//!   notional. Returned before signing; nothing was sent.

use crate::generated::behavior;

/// One row of the contract's error table. The generated `errors::TABLE` holds all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorInfo {
    pub code: u32,
    pub name: &'static str,
    pub status: u16,
    pub message: &'static str,
    pub retryable: bool,
}

/// The rate-limit code. Always retryable for reads.
pub const RATE_LIMIT_CODE: u32 = behavior::RATE_LIMIT_CODE;

/// The contract's error table, sorted by code. The generated client passes `errors::TABLE`.
#[derive(Debug, Clone, Copy)]
pub struct ErrorTable(pub &'static [ErrorInfo]);

impl ErrorTable {
    pub const EMPTY: ErrorTable = ErrorTable(&[]);

    pub fn get(&self, code: u32) -> Option<&'static ErrorInfo> {
        self.0.binary_search_by_key(&code, |e| e.code).ok().map(|i| &self.0[i])
    }

    /// The contract rule: the rate-limit code, a code the table marks retryable, or a read retry status.
    pub fn retryable(&self, code: u32, status: Option<u16>) -> bool {
        behavior::read_retryable(code, status, self.get(code).is_some_and(|e| e.retryable))
    }

    pub(crate) fn api_error(&self, code: u32, message: String, status: Option<u16>, rpc: Option<&str>) -> Error {
        let rpc = rpc.map(str::to_owned);
        if code == RATE_LIMIT_CODE {
            return Error::RateLimit { message, status, rpc };
        }
        Error::Api {
            code,
            retryable: self.retryable(code, status),
            message,
            status,
            rpc,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("{}: [{code}] {message}{}", rpc.as_deref().unwrap_or("?"), http(*status))]
    Api {
        code: u32,
        message: String,
        status: Option<u16>,
        rpc: Option<String>,
        retryable: bool,
    },

    #[error("{}: rate limited: {message}{}", rpc.as_deref().unwrap_or("?"), http(*status))]
    RateLimit {
        message: String,
        status: Option<u16>,
        rpc: Option<String>,
    },

    /// No usable answer. `sent` says whether the request may have left this process.
    #[error("{}{message}", rpc.as_deref().map(|r| format!("{r}: ")).unwrap_or_default())]
    Transport {
        message: String,
        rpc: Option<String>,
        sent: bool,
    },

    /// The encoded WS frame is larger than the server accepts. Nothing was sent.
    #[error("frame is {size} bytes, server limit {limit}; use OrderTransport::Rest for this call")]
    FrameTooLarge { size: usize, limit: usize },

    /// A write was in flight when the connection dropped or the call timed out. The order may or may
    /// not exist; look it up (e.g. by client_order_id). The SDK never resends a write.
    /// `client_order_ids` lists every order the write carried (several for bulk orders);
    /// `client_order_id` is set when there was exactly one.
    #[error("{rpc}: outcome unknown ({reason}){}", unknown_ids(client_order_ids))]
    OrderStateUnknown {
        rpc: String,
        reason: String,
        client_order_id: Option<u64>,
        client_order_ids: Vec<u64>,
        #[source]
        source: Option<Box<Error>>,
    },

    /// The consumer fell behind a stream that may not drop items. The queue was cleared and the
    /// subscription re-synced; items in between are lost.
    #[error("{stream} {selector}: consumer fell behind {capacity} items; re-syncing")]
    StreamOverflow {
        stream: String,
        selector: String,
        capacity: usize,
    },

    /// An order leg breaks one of its instrument's limits. Returned before signing; nothing was sent.
    #[error("{}", .0.message)]
    OrderOutOfBounds(Box<OrderOutOfBounds>),

    /// A request could not be signed (missing field, over-precise decimal, unknown metadata).
    /// Always raised before anything is signed or sent.
    #[error("signing: {0}")]
    Signing(String),

    /// The response did not match the expected type.
    #[error("{rpc}: cannot decode response: {message}")]
    Decode { rpc: String, message: String },

    /// The client is missing something the call needs (signer, api key) or was given a bad option.
    #[error("{0}")]
    Config(String),

    /// A cursor repeated before the traversal reached its end.
    #[error("pagination cursor repeated: {cursor:?}")]
    PaginationCycle { cursor: String },

    /// The client was closed.
    #[error("client closed")]
    Closed,
}

/// What `Error::OrderOutOfBounds` says about the failing leg. The SDK never rounds an order: pick one of
/// `nearest` and send again. Values are decimal strings without trailing zeros.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderOutOfBounds {
    pub instrument: String,
    /// Index of the failing leg.
    pub leg: usize,
    /// The leg field checked, or "limit_price * size" for the notional.
    pub field: String,
    /// "at_least" or "multiple_of".
    pub check: String,
    /// The instrument field it is checked against: tick_size, min_size or min_notional.
    pub limit: String,
    pub value: String,
    pub limit_value: String,
    /// The nearest valid values of `field` (of size, for the notional).
    pub nearest: Vec<String>,
    /// The gateway error the order would get, e.g. 2064 (invalid limit price tick).
    pub server_code: u32,
    pub message: String,
}

fn http(status: Option<u16>) -> String {
    status.map(|s| format!(" (http {s})")).unwrap_or_default()
}

impl Error {
    /// The server error code, for `Api` and `RateLimit`.
    pub fn code(&self) -> Option<u32> {
        match self {
            Error::Api { code, .. } => Some(*code),
            Error::RateLimit { .. } => Some(RATE_LIMIT_CODE),
            _ => None,
        }
    }

    /// Whether a read may be retried after this error.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Api { retryable, .. } => *retryable,
            Error::RateLimit { .. } => true,
            Error::Transport { .. } => true,
            _ => false,
        }
    }

    pub(crate) fn transport(message: impl Into<String>, rpc: Option<&str>, sent: bool) -> Error {
        Error::Transport {
            message: message.into(),
            rpc: rpc.map(str::to_owned),
            sent,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// `OrderStateUnknown` for a write that carried these client order ids.
    pub(crate) fn order_state_unknown(rpc: impl Into<String>, reason: impl Into<String>, ids: Vec<u64>) -> Self {
        Error::OrderStateUnknown {
            rpc: rpc.into(),
            reason: reason.into(),
            client_order_id: if ids.len() == 1 { Some(ids[0]) } else { None },
            client_order_ids: ids,
            source: None,
        }
    }
}

fn unknown_ids(ids: &[u64]) -> String {
    match ids {
        [] => String::new(),
        [one] => format!(", client_order_id={one}"),
        many => format!(", client_order_ids={many:?}"),
    }
}
