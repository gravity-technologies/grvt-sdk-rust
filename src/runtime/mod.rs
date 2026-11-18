//! Hand-written runtime. Fixed size: it does not grow with the API. Generated code
//! (`crate::generated`) talks to it only through the names listed in RUNTIME_API.md.
//!
//! Two layers:
//! - core (always built, no I/O): errors, codec, the lite encoding (`lite`), environments, specs, signing (`SignContext`,
//!   `SigningRequest`), request building and reply/frame parsing (`request`), session state
//!   (`session`), and the WebSocket protocol state (`protocol`: `RequestTable`, `Sequencer`, `Backoff`).
//! - client (feature `client`): `Runtime`, `Config`, `RpcCall` / `StreamCall`, sockets, HTTP transport,
//!   hooks. It does the I/O and calls the core for everything else.

pub mod clock;
pub mod codec;
pub mod env;
pub mod error;
pub mod http;
pub mod lite;
pub mod log;
pub mod protocol;
pub mod request;
pub mod session;
pub mod signing;
pub mod spec;

#[cfg(test)]
mod behavior_test;
#[cfg(test)]
pub(crate) mod lite_test;

#[cfg(feature = "client")]
pub mod auth;
#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
pub mod hooks;
#[cfg(feature = "client")]
pub mod rest;
#[cfg(feature = "client")]
pub mod testing;
#[cfg(feature = "client")]
pub mod ws;

pub use clock::{
    ClientOrderIdProvider, Clock, FixedClock, NonceProvider, RandomNonce, SystemClock, TimeCounterClientOrderId,
};
pub use env::Environment;
pub use error::{Error, ErrorInfo, ErrorTable, OrderOutOfBounds, Result};
pub use http::HttpResponse;
pub use lite::Encoding;
pub use protocol::{Backoff, Failed, GapRange, Pending, RequestTable, SeqState, Sequencer, Verdict};
pub use request::{
    api_error, decode_feed, decode_response, encode_request_into, may_retry, parse_subscribe_result, parse_ws_message,
    prepare_request, rest_request, rest_request_body, rest_response, rest_response_body, stale_instruments,
    subscribe_frame, transport_failure, unsubscribe_frame, ws_request, ws_request_into, ws_response, ws_result,
    HttpRequest, MessageKind, Request, RequestDefaults, SubscribeAck, WsError, WsMessage, READ_ATTEMPTS,
};
pub use session::{api_key_login_request, SessionState};
pub use signing::wire::{CurrencyMeta, InstrumentMeta, Meta};
pub use signing::{
    grvt_domain, hex0x, Domain, PrivateKeySigner, SignContext, Signature, SigningNeeds, SigningRequest, StructType,
    SyncSigner, Value,
};
pub use spec::{FeedFrame, Gateway, RpcOp, RpcSpec, StreamOp, StreamSpec};

#[cfg(feature = "client")]
pub use auth::Session;
#[cfg(feature = "client")]
pub use client::{Config, MetadataCache, MetadataLoaders, OrderTransport, RpcCall, Runtime, StreamCall};
#[cfg(feature = "client")]
pub use hooks::{Call, Direction, Event, EventKind, Hooks, Via};
#[cfg(feature = "client")]
pub use http::{HttpTransport, ReqwestTransport};
#[cfg(feature = "client")]
pub use signing::Signer;
#[cfg(feature = "client")]
pub use ws::{Subscription, WsSocket};

/// Trait impls for a generated enum with `as_str()` and `from_wire()`: Display, FromStr, serde
/// (as the wire string; unknown values decode into the fallback variant) and `ToScalar`.
#[doc(hidden)]
#[macro_export]
macro_rules! wire_enum {
    ($t:ident) => {
        impl ::std::fmt::Display for $t {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::std::str::FromStr for $t {
            type Err = ::std::convert::Infallible;
            fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
                Ok(Self::from_wire(s))
            }
        }

        impl ::serde::Serialize for $t {
            fn serialize<S: ::serde::Serializer>(&self, s: S) -> ::std::result::Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $t {
            fn deserialize<D: ::serde::Deserializer<'de>>(d: D) -> ::std::result::Result<Self, D::Error> {
                struct V;
                impl<'de> ::serde::de::Visitor<'de> for V {
                    type Value = $t;
                    fn expecting(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                        f.write_str(concat!("a ", stringify!($t), " string"))
                    }
                    fn visit_str<E: ::serde::de::Error>(self, v: &str) -> ::std::result::Result<$t, E> {
                        Ok($t::from_wire(v))
                    }
                }
                d.deserialize_str(V)
            }
        }

        impl $crate::runtime::signing::wire::ToScalar for $t {
            fn scalar(&self) -> $crate::runtime::signing::wire::Scalar<'_> {
                $crate::runtime::signing::wire::Scalar::Str(self.as_str())
            }
        }
    };
}
