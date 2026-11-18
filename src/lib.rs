//! Grvt SDK for Rust (draft): models, EIP-712 signing, request building and reply/frame parsing with no
//! I/O, and an async client (REST, WebSocket streams, WebSocket order entry, login) on top.
//!
//! Two layers in one crate:
//!
//! - **Core** (always built, no I/O, no async runtime): models, `signing::sign_<call>(&mut req, &ctx)`,
//!   `rest_request` / `ws_request` / `parse_ws_message` / `decode_feed` / `rest_response` /
//!   `ws_response`, `transport_failure`, session state, `Sequencer`, `RequestTable`, `Backoff`,
//!   `OrderBook`, recovery steps. For engines that own their sockets, threads and event loop.
//! - **Client** (feature `client`, on by default): `Grvt`, built on the core with tokio, reqwest and
//!   tokio-tungstenite. `default-features = false` leaves all of those out.
//!
//! Everything that grows with the API (models, enums, error codes, RPC/stream descriptors, sign
//! functions, client methods) is generated from `sdk/contract/contract.json` into `src/generated`. The
//! hand-written runtime in `src/runtime` stays the same size however many APIs exist.
//!
//! Without the client, one RPC is a descriptor (`ops::GetInstrument`) and two plain functions:
//!
//! ```
//! use grvt::{models, ops, Environment};
//!
//! let env = Environment::testnet();
//! let req = models::ApiGetInstrumentRequest::new("BTC_USDT_Perp");
//! let http = grvt::rest_request::<ops::GetInstrument>(&env, &req, None).unwrap();
//! assert_eq!(http.url, "https://market-data.testnet.grvt.io/full/v1/instrument");
//! // ... send it with your own HTTP client, then:
//! let err = grvt::rest_response::<ops::GetInstrument>(429, br#"{"code":1006,"message":"slow down"}"#, &[]);
//! assert!(matches!(err, Err(grvt::Error::RateLimit { .. })));
//! ```
//!
//! Models and enums are non-exhaustive so additive server changes do not force
//! downstream literal or match updates. Construct inputs with all required values
//! using new, then set optional values with typed with_* setters.
//!
//! ```
//! use grvt::models::OrderLeg;
//! let leg = OrderLeg::new("BTC_USDT_Perp", rust_decimal::Decimal::ONE, false);
//! assert_eq!(leg.instrument, "BTC_USDT_Perp");
//! ```
//!
//! An order leg requires an explicit buy/sell side:
//!
//! ```compile_fail,E0061
//! let leg = grvt::models::OrderLeg::new("BTC_USDT_Perp", rust_decimal::Decimal::ONE);
//! ```
//!
//! Struct update literals cannot be used outside this crate:
//!
//! ```compile_fail,E0639
//! let leg = grvt::models::OrderLeg { ..Default::default() };
//! ```
//!
//! Match enums with a wildcard even when handling today's Unknown variant:
//!
//! ```compile_fail,E0004
//! use grvt::enums::PositionMarginType;
//! fn describe(value: PositionMarginType) -> &'static str {
//!     match value {
//!         PositionMarginType::Isolated => "isolated",
//!         PositionMarginType::Cross => "cross",
//!         PositionMarginType::Unknown(_) => "unknown",
//!     }
//! }
//! ```
#![cfg_attr(
    feature = "client",
    doc = r#"
With the client:

```no_run
# async fn demo() -> grvt::Result<()> {
use futures::StreamExt;
use grvt::{Config, Grvt};

let client = Grvt::new(Config::testnet())?;
let inst = client.get_instrument().instrument("BTC_USDT_Perp").await?;
let mut book = client.orderbook_delta_stream().instrument("BTC_USDT_Perp").rate(100).await?;
while let Some(frame) = book.next().await {
    let frame = frame?;
    println!("{:?} {:?}", frame.sequence_number, frame.feed.bids);
}
# Ok(()) }
```

Authentication belongs to the session runtime, not generated RPC methods.

```compile_fail
let client = grvt::Grvt::new(grvt::Config::testnet()).unwrap();
let _ = client.login_api_key();
```

```compile_fail
let client = grvt::Grvt::new(grvt::Config::testnet()).unwrap();
let _ = client.login_wallet();
```

There is no blocking client: the client is async (tokio). Without an async runtime, use the core.
"#
)]

#[rustfmt::skip]
mod generated;
pub mod backfill;
pub mod countdown;
pub mod orderbook;
pub mod pagination;
pub mod recovery;
pub mod runtime;
pub mod scalars;

pub use backfill::{backfill_status, BackfillPage, BackfillStatus};
pub use countdown::{cancel_countdown_request, countdown_receipt, CountdownReceipt};
pub use orderbook::{BookError, BookMode, BookSnapshot, OrderBook};
pub use pagination::{Page, PageCursor, PageStep};
pub use recovery::{resolve_order_request, resolve_order_result, OrderResolution};
pub use scalars::Timestamp;

pub use generated::{enums, errors as error_codes, models, ops, signing, specs, CONTRACT_VERSION};
pub use runtime::{
    api_error, api_key_login_request, decode_feed, decode_response, encode_request_into, may_retry,
    parse_subscribe_result, parse_ws_message, prepare_request, rest_request, rest_response, stale_instruments,
    subscribe_frame, transport_failure, unsubscribe_frame, ws_request, ws_request_into, ws_response, ws_result,
    Backoff, CurrencyMeta, Domain, Encoding, Environment, Error, FeedFrame, GapRange, HttpRequest, HttpResponse,
    InstrumentMeta, MessageKind, Meta, PrivateKeySigner, RequestDefaults, RequestTable, Result, RpcOp, SeqState,
    Sequencer, SessionState, SignContext, Signature, SigningNeeds, SigningRequest, StreamOp, StructType, SubscribeAck,
    SyncSigner, Value, Verdict, WsMessage,
};

#[cfg(feature = "client")]
pub use backfill::backfill_pages;
#[cfg(feature = "client")]
pub use countdown::refresh_cancel_countdown;
#[cfg(feature = "client")]
pub use generated::api;
#[cfg(feature = "client")]
pub use generated::client::Grvt;
#[cfg(feature = "client")]
pub use pagination::pages;
#[cfg(feature = "client")]
pub use recovery::resolve_order;
#[cfg(feature = "client")]
pub use runtime::{Call, Config, Event, EventKind, OrderTransport, Signer, Subscription, Via};
