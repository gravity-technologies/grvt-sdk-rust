//! Descriptors the generator emits, one per RPC and one per stream. The runtime reads only these.

use super::lite::{Encoding, LiteDecode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Gateway {
    /// Market data ("md").
    MarketData,
    /// Trading ("td").
    Trading,
    /// Edge (login).
    Edge,
}

impl Gateway {
    pub fn short(self) -> &'static str {
        match self {
            Gateway::MarketData => "md",
            Gateway::Trading => "td",
            Gateway::Edge => "edge",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcSpec {
    /// JSON-RPC method and REST route tail, e.g. "v1/create_order".
    pub method: &'static str,
    pub gateway: Gateway,
    /// REST path, e.g. "/full/v1/create_order" (edge routes differ).
    pub path: &'static str,
    /// REST path in the lite encoding, e.g. "/lite/v1/create_order"; empty for edge routes (no lite form).
    pub lite_path: &'static str,
    /// Needs the session cookie.
    pub auth: bool,
    /// Changes state: never retried; an ambiguous failure is `Error::OrderStateUnknown`.
    pub write: bool,
    /// Goes over the trading WS when the client uses `OrderTransport::Ws`.
    pub order_entry: bool,
    /// Error codes that mean the cached instrument limits may be stale: the runtime drops the request's
    /// leg instruments from the metadata cache. The order itself is never resent.
    pub refresh_metadata_on: &'static [u32],
}

impl RpcSpec {
    /// The encoding this call uses on a client set to `encoding`: edge routes are always full.
    pub fn encoding(&self, encoding: Encoding) -> Encoding {
        if self.lite_path.is_empty() {
            Encoding::Full
        } else {
            encoding
        }
    }

    /// The REST path in `encoding` (edge routes keep their full path).
    pub fn path_for(&self, encoding: Encoding) -> &'static str {
        match self.encoding(encoding) {
            Encoding::Full => self.path,
            Encoding::Lite => self.lite_path,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamSpec {
    /// e.g. "v1.book.d"
    pub stream: &'static str,
    pub gateway: Gateway,
    pub auth: bool,
    /// Every frame is a full snapshot (e.g. "v1.ticker.s"): a gap is reported but needs no resync.
    pub snapshot_feed: bool,
    /// Old items may be dropped when the consumer is slow: snapshot streams only. Every other stream
    /// (deltas, events, private) gets `Error::StreamOverflow` and a re-sync instead.
    pub conflate: bool,
}

/// One RPC as a type: `grvt::ops::CreateOrder` and friends (generated). The core request and reply
/// functions take it as a type parameter: `rest_request::<ops::CreateOrder>(&env, &req, Some(&session))`.
pub trait RpcOp {
    type Request: super::request::Request;
    type Response: serde::de::DeserializeOwned + LiteDecode;
    const SPEC: &'static RpcSpec;
}

/// One stream as a type: `grvt::ops::OrderbookDeltaStream` and friends (generated).
pub trait StreamOp {
    /// The selector model, e.g. `models::WSOrderbookLevelsFeedSelectorV1`.
    type Selector;
    /// The frame model, e.g. `models::WSOrderbookLevelsFeedDataV1`.
    type Feed: FeedFrame;
    const SPEC: &'static StreamSpec;
    /// The selector string the subscribe frame carries, e.g. "BTC_USDT_Perp@500-10".
    fn selector(selector: &Self::Selector) -> String;
}

/// A stream frame model built from a parsed message: the routing fields come from the one parse
/// (`parse_ws_message`), only the `feed` payload is decoded. Generated for every frame model.
pub trait FeedFrame: Sized {
    fn from_message(message: &super::request::WsMessage<'_>) -> serde_json::Result<Self>;
}
