//! Requests as plain data and replies classified into `Error`, for an application's own HTTP and
//! WebSocket code. No I/O. A write over REST:
//!
//! ```text
//! let ids = prepare_request(ops::CreateOrder::SPEC, &mut req, &defaults)?;   // sub-account, client order ids
//! signing::sign_order_create_order(&mut req, &ctx)?;                          // EIP-712 signature
//! let http = rest_request::<ops::CreateOrder>(&env, &req, Some(&session))?;
//! ... the application sends http ...
//! on a reply:         rest_response::<ops::CreateOrder>(status, &body, &[])
//! on a send failure:  transport_failure(ops::CreateOrder::SPEC, sent, why, ids)
//! ```
//!
//! REST success is HTTP 200 with the response object as the body; failure is `{"code", "message",
//! "status"}`. Over WS every RPC is JSON-RPC 2.0 with method = `spec.method` and params = the request
//! object; stream frames arrive without an envelope: `{stream, selector, sequence_number,
//! prev_sequence_number, feed}`.
//!
//! The free functions speak the full encoding. For lite, call the same names on `Encoding::Lite`
//! (`Encoding::Lite.rest_request::<ops::GetInstrument>(&env, &req, None)`): the lite path, lite request
//! and reply bodies, the lite JSON-RPC envelope and stream frames. A parsed `WsMessage` remembers its
//! encoding, so `ws_response` and `decode_feed` need no argument.

use std::borrow::Cow;
use std::sync::LazyLock;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::clock::{ClientOrderIdProvider, TimeCounterClientOrderId};
use super::codec::{opt_cow_str, opt_num, opt_u64, ErrorBody, LiteErrorBody, LiteSubscribeResult, SubscribeResult};
use super::env::Environment;
use super::error::{Error, ErrorTable, Result};
use super::lite::{Encoding, LiteDecode, LiteEncode};
use super::session::SessionState;
use super::spec::{FeedFrame, RpcOp, RpcSpec, StreamOp, StreamSpec};
use crate::generated::behavior;

/// Attempts for a read that keeps failing with a retryable error. Writes: one.
pub const READ_ATTEMPTS: u32 = behavior::READ_ATTEMPTS;

fn errors() -> ErrorTable {
    crate::error_codes::TABLE
}

/// Implemented by every generated request type.
pub trait Request: Serialize + DeserializeOwned + LiteEncode + Send {
    /// Fill the sub-account when the request has one and it is unset. True when it was filled.
    fn default_account(&mut self, _account: Option<u64>) -> bool {
        false
    }
    fn required_fields() -> &'static [&'static str] {
        &[]
    }
    /// The first required field that is not set.
    fn missing_field(&self) -> Option<&'static str> {
        None
    }

    /// Order writes: set each order's `client_order_id` when absent and return them all (one for
    /// create_order, one per order for bulk orders), so an ambiguous failure can report them. Other
    /// requests return an empty list.
    fn fill_client_order_ids(&mut self, _ids: &dyn ClientOrderIdProvider) -> Vec<u64> {
        Vec::new()
    }
}

/// What `prepare_request` fills in.
#[derive(Clone, Copy, Default)]
pub struct RequestDefaults<'a> {
    /// For requests (and orders) without a sub-account. Explicit ids win.
    pub sub_account_id: Option<u64>,
    /// Client order ids for orders without one; `None` uses a process-wide `TimeCounterClientOrderId`.
    pub client_order_ids: Option<&'a dyn ClientOrderIdProvider>,
}

impl std::fmt::Debug for RequestDefaults<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestDefaults")
            .field("sub_account_id", &self.sub_account_id)
            .field("client_order_ids", &self.client_order_ids.is_some())
            .finish()
    }
}

/// Fill the sub-account and client order ids, then check required fields. Returns the request's client
/// order ids (one per order; empty for requests that carry no order): keep them, `transport_failure` needs
/// them if the answer is lost.
pub fn prepare_request<R: Request>(spec: &RpcSpec, req: &mut R, defaults: &RequestDefaults<'_>) -> Result<Vec<u64>> {
    static IDS: LazyLock<TimeCounterClientOrderId> = LazyLock::new(TimeCounterClientOrderId::default);
    req.default_account(defaults.sub_account_id);
    if let Some(field) = req.missing_field() {
        return Err(Error::Config(format!("{}: {field} is required", spec.method)));
    }
    Ok(req.fill_client_order_ids(defaults.client_order_ids.unwrap_or(&*IDS)))
}

/// An `Api` or `RateLimit` error for a server error code, classified by the contract's error table.
pub fn api_error(code: u32, message: impl Into<String>, status: Option<u16>, rpc: Option<&str>) -> Error {
    errors().api_error(code, message.into(), status, rpc)
}

/// Whether a failed call may be sent again: never for a write, else `Error::is_retryable`.
pub fn may_retry(spec: &RpcSpec, error: &Error) -> bool {
    !spec.write && error.is_retryable()
}

/// The application could not get an answer. `sent`: the request may have left the process (anything but
/// a connect failure). A write that may have been sent becomes `OrderStateUnknown` with its client order
/// ids; look the orders up (`resolve_order_request`) instead of resending. `client_order_ids` is what
/// `prepare_request` returned for this request; pass an empty list for reads and for writes that create no
/// orders.
pub fn transport_failure(spec: &RpcSpec, sent: bool, message: impl Into<String>, client_order_ids: Vec<u64>) -> Error {
    if spec.write && sent {
        write_outcome(
            spec,
            Error::transport(message, Some(spec.method), true),
            client_order_ids,
        )
    } else {
        Error::transport(message, Some(spec.method), sent)
    }
}

/// Retain request ids when a write received no trustworthy outcome. Known contract rejections
/// stay API errors; server failures and unknown response shapes need reconciliation.
pub fn write_outcome(spec: &RpcSpec, error: Error, ids: Vec<u64>) -> Error {
    let code_status = |code: u32| errors().get(code).map(|info| info.status);
    let ambiguous = match &error {
        Error::Transport { sent, .. } => *sent,
        Error::Decode { .. } => true,
        Error::RateLimit { status, .. } => {
            behavior::write_outcome_unknown(*status, code_status(behavior::RATE_LIMIT_CODE))
        }
        Error::Api { code, status, .. } => behavior::write_outcome_unknown(*status, code_status(*code)),
        _ => false,
    };
    if spec.write && ambiguous {
        let mut out = Error::order_state_unknown(spec.method, error.to_string(), ids);
        if let Error::OrderStateUnknown { source, .. } = &mut out {
            *source = Some(Box::new(error));
        }
        out
    } else {
        error
    }
}

/// Instruments named by the legs of an encoded order request, when `error` is one of the spec's
/// `refresh_metadata_on` codes: their limits may be stale, reload them before the next order.
pub fn stale_instruments(spec: &RpcSpec, error: &Error, request_body: &[u8]) -> Vec<String> {
    match error.code() {
        Some(c) if spec.refresh_metadata_on.contains(&c) => {
            super::signing::checks::leg_instruments(request_body, "legs", "instrument")
        }
        _ => Vec::new(),
    }
}

/// Decode a full-encoding reply body into `T`; a mismatch is `Error::Decode` named after the call.
pub fn decode_response<T: DeserializeOwned + LiteDecode>(spec: &RpcSpec, body: &[u8]) -> Result<T> {
    Encoding::Full.decode_response(spec, body)
}

fn encode_error(e: serde_json::Error) -> Error {
    Error::Config(format!("cannot encode request: {e}"))
}

// -- REST ---------------------------------------------------------------------------------------

/// A request for the application to send as is.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// "POST" or "GET".
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for HttpRequest {
    // the cookie and api keys are redacted, like the wire logs
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<_> = self
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), super::log::redact_header(k, v)))
            .collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body", &super::log::redact(&self.body, 4000))
            .finish()
    }
}

/// POST {gateway}{path} with the encoded request. An auth call needs a logged-in session.
pub fn rest_request<O: RpcOp>(
    env: &Environment,
    req: &O::Request,
    session: Option<&SessionState>,
) -> Result<HttpRequest> {
    Encoding::Full.rest_request::<O>(env, req, session)
}

/// `rest_request` for a body already encoded.
pub fn rest_request_body(
    spec: &RpcSpec,
    env: &Environment,
    body: Vec<u8>,
    session: Option<&SessionState>,
) -> Result<HttpRequest> {
    Encoding::Full.rest_request_body(spec, env, body, session)
}

/// The body of a 200, or the classified error.
pub fn rest_response_body<'b>(spec: &RpcSpec, status: u16, body: &'b [u8]) -> Result<&'b [u8]> {
    Encoding::Full.rest_response_body(spec, status, body)
}

/// The response of a REST reply, or the classified error.
pub fn rest_response<O: RpcOp>(status: u16, body: &[u8], ids: &[u64]) -> Result<O::Response> {
    Encoding::Full.rest_response::<O>(status, body, ids)
}

/// The core request and reply functions in one encoding; the free functions of the same names are
/// `Encoding::Full`'s. Edge routes always use full names, whatever the encoding.
impl Encoding {
    /// Decode a reply body into `T`; a mismatch is `Error::Decode` named after the call.
    pub fn decode_response<T: DeserializeOwned + LiteDecode>(self, spec: &RpcSpec, body: &[u8]) -> Result<T> {
        spec.encoding(self).from_slice(body).map_err(|e| Error::Decode {
            rpc: spec.method.into(),
            message: e.to_string(),
        })
    }

    /// POST {gateway}{path} with the encoded request. An auth call needs a logged-in session.
    pub fn rest_request<O: RpcOp>(
        self,
        env: &Environment,
        req: &O::Request,
        session: Option<&SessionState>,
    ) -> Result<HttpRequest> {
        let body = O::SPEC.encoding(self).to_vec(req).map_err(encode_error)?;
        self.rest_request_body(O::SPEC, env, body, session)
    }

    /// `rest_request` for a body already encoded in this encoding.
    pub fn rest_request_body(
        self,
        spec: &RpcSpec,
        env: &Environment,
        body: Vec<u8>,
        session: Option<&SessionState>,
    ) -> Result<HttpRequest> {
        rest_request_at(spec, env, spec.path_for(self), body, session)
    }

    /// The body of a 200, or the classified error.
    pub fn rest_response_body<'b>(self, spec: &RpcSpec, status: u16, body: &'b [u8]) -> Result<&'b [u8]> {
        if status == 200 {
            return Ok(body);
        }
        let parsed = match spec.encoding(self) {
            Encoding::Full => serde_json::from_slice::<ErrorBody>(body),
            Encoding::Lite => serde_json::from_slice::<LiteErrorBody>(body).map(ErrorBody::from),
        };
        let (code, message) = match parsed {
            Ok(ErrorBody { code, message }) => (code, message),
            _ => (0, String::from_utf8_lossy(&body[..body.len().min(200)]).into_owned()),
        };
        Err(errors().api_error(code, message, Some(status), Some(spec.method)))
    }

    /// The response of a REST reply, or the classified error.
    pub fn rest_response<O: RpcOp>(self, status: u16, body: &[u8], ids: &[u64]) -> Result<O::Response> {
        self.rest_response_body(O::SPEC, status, body)
            .and_then(|raw| self.decode_response(O::SPEC, raw))
            .map_err(|e| write_outcome(O::SPEC, e, ids.to_vec()))
    }

    /// The JSON-RPC frame for one call.
    pub fn ws_request<O: RpcOp>(self, req: &O::Request, id: u32) -> Result<String> {
        let params = self.to_vec(req).map_err(encode_error)?;
        Ok(self.jsonrpc_frame(O::SPEC.method, &params, id))
    }

    /// `ws_request` into a buffer the caller keeps: cleared, then filled. Once the buffer has grown, encoding
    /// allocates nothing. On an error its contents are unspecified.
    pub fn ws_request_into<O: RpcOp>(self, req: &O::Request, id: u32, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        let (open, params, close) = self.envelope_keys();
        out.extend_from_slice(open.as_bytes());
        out.extend_from_slice(O::SPEC.method.as_bytes());
        out.extend_from_slice(params.as_bytes());
        self.write(req, out).map_err(encode_error)?;
        out.extend_from_slice(close.as_bytes());
        // Writing into a Vec cannot fail.
        let _ = std::io::Write::write_fmt(out, format_args!("{id}}}"));
        Ok(())
    }

    /// The request's JSON in this encoding into a buffer the caller keeps: cleared, then filled.
    pub fn encode_request_into<R: Request>(self, req: &R, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        self.write(req, out).map_err(encode_error)
    }

    /// The JSON-RPC 2.0 envelope's keys around the method, params and id: `JSONRPCRequest` `j m p i` in lite.
    fn envelope_keys(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Encoding::Full => (r#"{"jsonrpc":"2.0","method":""#, r#"","params":"#, r#","id":"#),
            Encoding::Lite => (r#"{"j":"2.0","m":""#, r#"","p":"#, r#","i":"#),
        }
    }

    /// Build the request text without re-encoding already-encoded params.
    pub fn jsonrpc_frame(self, method: &str, params_json: &[u8], id: u32) -> String {
        let params = std::str::from_utf8(params_json).unwrap_or("{}");
        let (open, p, close) = self.envelope_keys();
        let mut out = String::with_capacity(open.len() + method.len() + p.len() + params.len() + close.len() + 11);
        for part in [open, method, p, params, close] {
            out.push_str(part);
        }
        // Writing into a String cannot fail.
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{id}}}"));
        out
    }

    /// Parse a frame once (see the free `parse_ws_message`). The message keeps this encoding for
    /// `ws_response` and `decode_feed`.
    pub fn parse_ws_message(self, text: &[u8]) -> Result<WsMessage<'_>> {
        // serde would read a JSON array as a struct too ([5, {...}] as a reply with id 5)
        if text.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
            return Err(Error::Decode {
                rpc: "websocket frame".into(),
                message: "not a JSON object".into(),
            });
        }
        let e: Envelope<'_> = match self {
            Encoding::Full => serde_json::from_slice(text),
            Encoding::Lite => serde_json::from_slice::<LiteEnvelope<'_>>(text).map(Envelope::from),
        }
        .map_err(|e| Error::Decode {
            rpc: "websocket frame".into(),
            message: e.to_string(),
        })?;
        Ok(message_from(self, e, text))
    }

    /// The subscribe frame. Subscriptions use global sequence numbers (see `Sequencer`).
    pub fn subscribe_frame(self, spec: &StreamSpec, selector: &str, id: u32) -> String {
        self.jsonrpc_frame("subscribe", &self.subscription_params(spec, selector), id)
    }

    pub fn unsubscribe_frame(self, spec: &StreamSpec, selector: &str, id: u32) -> String {
        self.jsonrpc_frame("unsubscribe", &self.subscription_params(spec, selector), id)
    }

    /// `WSSubscribeParams`: `stream selectors use_global_sequence_number`, `s s1 ug` in lite.
    fn subscription_params(self, spec: &StreamSpec, selector: &str) -> Vec<u8> {
        let (stream, selectors, global) = match self {
            Encoding::Full => ("stream", "selectors", "use_global_sequence_number"),
            Encoding::Lite => ("s", "s1", "ug"),
        };
        let mut params = serde_json::Map::new();
        params.insert(stream.into(), spec.stream.into());
        params.insert(selectors.into(), serde_json::json!([selector]));
        params.insert(global.into(), true.into());
        serde_json::to_vec(&params).expect("json")
    }

    /// A subscribe reply's result (`WSSubscribeResult`).
    pub fn parse_subscribe_result(self, result_json: &str, requested_selector: &str) -> Result<SubscribeAck> {
        let r: SubscribeResult = match self {
            Encoding::Full => serde_json::from_str(result_json),
            Encoding::Lite => serde_json::from_str::<LiteSubscribeResult>(result_json).map(SubscribeResult::from),
        }
        .map_err(|e| Error::Decode {
            rpc: "subscribe".into(),
            message: e.to_string(),
        })?;
        Ok(SubscribeAck {
            selector: r.subs.first().cloned().unwrap_or_else(|| requested_selector.to_owned()),
            latest_sequence_number: r.latest_sequence_number.0.first().copied().unwrap_or(0),
            num_snapshots: r.num_snapshots.0.first().copied().unwrap_or(0),
        })
    }
}

fn rest_request_at(
    spec: &RpcSpec,
    env: &Environment,
    path: &str,
    body: Vec<u8>,
    session: Option<&SessionState>,
) -> Result<HttpRequest> {
    let mut headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
    if spec.auth {
        match session.filter(|s| s.logged_in()) {
            Some(s) => headers.extend(s.headers()),
            None => {
                return Err(Error::Config(format!(
                    "{}: this call needs a session: log in first",
                    spec.method
                )))
            }
        }
    }
    Ok(HttpRequest {
        method: "POST",
        url: env.rest_url(spec.gateway, path),
        headers,
        body,
    })
}

// -- WebSocket ----------------------------------------------------------------------------------

/// The JSON-RPC frame for one call: `{"jsonrpc":"2.0","method":...,"params":<request>,"id":...}`.
pub fn ws_request<O: RpcOp>(req: &O::Request, id: u32) -> Result<String> {
    Encoding::Full.ws_request::<O>(req, id)
}

/// `ws_request` into a buffer the caller keeps: cleared, then filled with the same bytes `ws_request`
/// returns. Once the buffer has grown, encoding allocates nothing. On an error its contents are unspecified.
pub fn ws_request_into<O: RpcOp>(req: &O::Request, id: u32, out: &mut Vec<u8>) -> Result<()> {
    Encoding::Full.ws_request_into::<O>(req, id, out)
}

/// The request's JSON (the body `rest_request` sends) into a buffer the caller keeps: cleared, then
/// filled. Once the buffer has grown, encoding allocates nothing. On an error its contents are unspecified.
pub fn encode_request_into<R: Request>(req: &R, out: &mut Vec<u8>) -> Result<()> {
    Encoding::Full.encode_request_into(req, out)
}

/// What a WS text frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    /// A JSON-RPC reply: `id` is set.
    Reply,
    /// A stream frame: `stream` and `selector` are set.
    Feed,
    /// Anything else (dropped by the client).
    Other,
}

/// An error a reply carries (`error`, or a REST-style `{code, message}` without a result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsError {
    pub code: u32,
    pub message: String,
}

/// One incoming WS text frame, parsed once: what routes it (id, stream, selector, sequence numbers) and
/// the payload as a borrowed raw slice, decoded only when asked (`ws_response`, `decode_feed`).
#[derive(Debug, Clone)]
pub struct WsMessage<'a> {
    pub kind: MessageKind,
    pub id: Option<u32>,
    pub stream: Option<Cow<'a, str>>,
    pub selector: Option<Cow<'a, str>>,
    pub sequence_number: Option<u64>,
    pub prev_sequence_number: Option<u64>,
    /// A reply's raw `result` JSON.
    pub result: Option<&'a str>,
    pub error: Option<WsError>,
    /// The reply carried an error envelope that did not match the contract.
    pub malformed_error: bool,
    /// A stream frame's raw `feed` JSON.
    pub feed: Option<&'a str>,
    /// The whole frame as received.
    pub text: &'a [u8],
    /// The encoding the frame was parsed in; its result and feed are decoded in it.
    pub encoding: Encoding,
}

#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(default, with = "opt_num")]
    id: Option<u32>,
    #[serde(borrow, default)]
    result: Option<&'a RawValue>,
    #[serde(borrow, default, deserialize_with = "present_raw")]
    error: Option<&'a RawValue>,
    #[serde(borrow, default, deserialize_with = "opt_cow_str")]
    stream: Option<Cow<'a, str>>,
    #[serde(borrow, default, deserialize_with = "opt_cow_str")]
    selector: Option<Cow<'a, str>>,
    #[serde(default, with = "opt_u64")]
    sequence_number: Option<u64>,
    #[serde(default, with = "opt_u64")]
    prev_sequence_number: Option<u64>,
    #[serde(borrow, default)]
    feed: Option<&'a RawValue>,
    /// REST-style error body (`{"code","message","status"}`) sometimes seen on WS too.
    #[serde(borrow, default, deserialize_with = "present_raw")]
    code: Option<&'a RawValue>,
}

// Preserve explicit null as a present, malformed error rather than a missing field.
fn present_raw<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<&'de RawValue>, D::Error> {
    <&RawValue>::deserialize(d).map(Some)
}

/// `Envelope` with lite names: `JSONRPCResponse` `i r e`, stream frames `s s1 sn ps f`, and a REST-style
/// error's `c`. `s` is also a REST-style error's status, so a non-string `s` is not a stream.
#[derive(Deserialize)]
struct LiteEnvelope<'a> {
    #[serde(rename = "i", default, with = "opt_num")]
    id: Option<u32>,
    #[serde(rename = "r", borrow, default)]
    result: Option<&'a RawValue>,
    #[serde(rename = "e", borrow, default, deserialize_with = "present_raw")]
    error: Option<&'a RawValue>,
    #[serde(rename = "s", borrow, default, deserialize_with = "lenient_cow_str")]
    stream: Option<Cow<'a, str>>,
    #[serde(rename = "s1", borrow, default, deserialize_with = "opt_cow_str")]
    selector: Option<Cow<'a, str>>,
    #[serde(rename = "sn", default, with = "opt_u64")]
    sequence_number: Option<u64>,
    #[serde(rename = "ps", default, with = "opt_u64")]
    prev_sequence_number: Option<u64>,
    #[serde(rename = "f", borrow, default)]
    feed: Option<&'a RawValue>,
    #[serde(rename = "c", borrow, default, deserialize_with = "present_raw")]
    code: Option<&'a RawValue>,
}

impl<'a> From<LiteEnvelope<'a>> for Envelope<'a> {
    fn from(e: LiteEnvelope<'a>) -> Self {
        Envelope {
            id: e.id,
            result: e.result,
            error: e.error,
            stream: e.stream,
            selector: e.selector,
            sequence_number: e.sequence_number,
            prev_sequence_number: e.prev_sequence_number,
            feed: e.feed,
            code: e.code,
        }
    }
}

/// A string borrowed when possible; any other JSON value is `None`.
fn lenient_cow_str<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Cow<'de, str>>, D::Error> {
    use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<Cow<'de, str>>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("any JSON value")
        }
        fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> std::result::Result<Self::Value, E> {
            Ok(Some(Cow::Borrowed(v)))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
            Ok(Some(Cow::Owned(v.to_owned())))
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> std::result::Result<Self::Value, A::Error> {
            while a.next_element::<IgnoredAny>()?.is_some() {}
            Ok(None)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> std::result::Result<Self::Value, A::Error> {
            while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(None)
        }
    }
    d.deserialize_any(V)
}

/// Parse a frame once. A frame that cannot be routed or read (not an object, an id over uint32, a
/// unrouteable envelope) is `Error::Decode`: drop it. Malformed nested errors keep the reply id so
/// `ws_result` can promptly fail the matching request. A stream frame's payload is only checked when `decode_feed` reads it.
pub fn parse_ws_message(text: &[u8]) -> Result<WsMessage<'_>> {
    Encoding::Full.parse_ws_message(text)
}

fn message_from<'a>(encoding: Encoding, e: Envelope<'a>, text: &'a [u8]) -> WsMessage<'a> {
    let error_body = |raw: &str| match encoding {
        Encoding::Full => serde_json::from_str::<ErrorBody>(raw),
        Encoding::Lite => serde_json::from_str::<LiteErrorBody>(raw).map(ErrorBody::from),
    };
    let kind = match (e.id, &e.stream, &e.selector) {
        (None, Some(_), Some(_)) => MessageKind::Feed,
        (Some(_), _, _) => MessageKind::Reply,
        _ => MessageKind::Other,
    };
    let mut malformed_error = false;
    let error = if let Some(raw) = e.error {
        match error_body(raw.get()) {
            Ok(b) => Some(WsError {
                code: b.code,
                message: b.message,
            }),
            Err(_) => {
                malformed_error = true;
                None
            }
        }
    } else if e.code.is_some_and(|raw| {
        let code: std::result::Result<Option<u32>, _> =
            opt_num::deserialize(&mut serde_json::Deserializer::from_str(raw.get()));
        !matches!(code, Ok(Some(0)) | Ok(None))
    }) && e.result.is_none()
    {
        match std::str::from_utf8(text)
            .map_err(<serde_json::Error as serde::de::Error>::custom)
            .and_then(error_body)
        {
            Ok(body) => Some(WsError {
                code: body.code,
                message: body.message,
            }),
            Err(_) => {
                malformed_error = true;
                None
            }
        }
    } else {
        None
    };
    WsMessage {
        kind,
        id: e.id,
        stream: e.stream,
        selector: e.selector,
        sequence_number: e.sequence_number,
        prev_sequence_number: e.prev_sequence_number,
        result: e.result.map(RawValue::get),
        error,
        malformed_error,
        feed: e.feed.map(RawValue::get),
        text,
        encoding,
    }
}

/// The raw `result` of a reply ("null" when absent), or its classified error. `rpc` names the request's
/// method.
pub fn ws_result<'a>(reply: &WsMessage<'a>, rpc: &str) -> Result<&'a str> {
    if reply.malformed_error {
        return Err(Error::Decode {
            rpc: rpc.into(),
            message: "malformed error reply".into(),
        });
    }
    if let Some(e) = &reply.error {
        return Err(errors().api_error(e.code, e.message.clone(), None, Some(rpc)));
    }
    Ok(reply.result.unwrap_or("null"))
}

/// The response of a WS reply, or the classified error.
pub fn ws_response<O: RpcOp>(reply: &WsMessage<'_>, ids: &[u64]) -> Result<O::Response> {
    ws_result(reply, O::SPEC.method)
        .and_then(|raw| reply.encoding.decode_response(O::SPEC, raw.as_bytes()))
        .map_err(|e| write_outcome(O::SPEC, e, ids.to_vec()))
}

/// The subscribe frame. Subscriptions use global sequence numbers (see `Sequencer`).
pub fn subscribe_frame(spec: &StreamSpec, selector: &str, id: u32) -> String {
    Encoding::Full.subscribe_frame(spec, selector, id)
}

pub fn unsubscribe_frame(spec: &StreamSpec, selector: &str, id: u32) -> String {
    Encoding::Full.unsubscribe_frame(spec, selector, id)
}

/// A subscribe reply's result. `selector` is the server's canonical form of the requested selector (feed
/// frames carry it). Snapshot frames (sequence_number 0) follow, then deltas newer than
/// `latest_sequence_number`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscribeAck {
    pub selector: String,
    pub latest_sequence_number: u64,
    pub num_snapshots: u64,
}

pub fn parse_subscribe_result(result_json: &str, requested_selector: &str) -> Result<SubscribeAck> {
    Encoding::Full.parse_subscribe_result(result_json, requested_selector)
}

/// Decode a stream frame: the routing fields come from `feed`'s one parse, only the payload is decoded.
pub fn decode_feed<S: StreamOp>(feed: &WsMessage<'_>) -> Result<S::Feed> {
    decode_frame_as(S::SPEC.stream, feed)
}

pub(crate) fn decode_frame_as<F: FeedFrame>(stream: &str, feed: &WsMessage<'_>) -> Result<F> {
    F::from_message(feed).map_err(|e| Error::Decode {
        rpc: stream.into(),
        message: e.to_string(),
    })
}

impl<'a> WsMessage<'a> {
    #[doc(hidden)]
    pub fn missing(field: &'static str) -> serde_json::Error {
        <serde_json::Error as serde::de::Error>::missing_field(field)
    }

    #[doc(hidden)]
    pub fn feed_as<T: Deserialize<'a> + LiteDecode>(&self) -> serde_json::Result<T> {
        self.encoding.from_str(self.feed.ok_or_else(|| Self::missing("feed"))?)
    }

    #[doc(hidden)]
    pub fn feed_opt_as<T: Deserialize<'a> + LiteDecode>(&self) -> serde_json::Result<Option<T>> {
        Ok(match self.feed {
            Some(f) => self.encoding.from_str::<Option<T>>(f)?,
            None => None,
        })
    }

    /// The whole frame as `T`, for frame models with fields beyond the routing ones.
    #[doc(hidden)]
    pub fn whole_as<T: DeserializeOwned + LiteDecode>(&self) -> serde_json::Result<T> {
        self.encoding.from_slice(self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_replies_feeds_and_errors_once() {
        let f = parse_ws_message(br#"{"stream":"v1.book.d","selector":"X@500","sequence_number":"7","prev_sequence_number":"6","feed":{"a":1}}"#).unwrap();
        assert_eq!(f.kind, MessageKind::Feed);
        assert_eq!((f.sequence_number, f.prev_sequence_number), (Some(7), Some(6)));
        assert_eq!(f.feed, Some(r#"{"a":1}"#));
        assert!(matches!(f.stream, Some(Cow::Borrowed("v1.book.d"))));
        let r = parse_ws_message(br#"{"jsonrpc":"2.0","id":3,"result":{"x":[1,2]}}"#).unwrap();
        assert_eq!((r.kind, r.id), (MessageKind::Reply, Some(3)));
        assert_eq!(ws_result(&r, "v1/x").unwrap(), r#"{"x":[1,2]}"#);
        let e = parse_ws_message(br#"{"id":4,"error":{"code":1006,"message":"slow down"}}"#).unwrap();
        assert!(matches!(ws_result(&e, "v1/x"), Err(Error::RateLimit { .. })));
        let rest_style = parse_ws_message(br#"{"id":5,"code":2064,"message":"tick","status":400}"#).unwrap();
        assert!(matches!(
            ws_result(&rest_style, "v1/x"),
            Err(Error::Api { code: 2064, .. })
        ));
        for raw in [
            br#"{"id":7,"error":{"code":1003},"result":{"ok":true}}"#.as_slice(),
            br#"{"id":7,"code":1003}"#.as_slice(),
            br#"{"id":7,"error":null,"result":{"ok":true}}"#.as_slice(),
            br#"{"id":7,"error":false}"#.as_slice(),
        ] {
            let malformed = parse_ws_message(raw).unwrap();
            assert_eq!(malformed.id, Some(7));
            assert!(matches!(ws_result(&malformed, "write"), Err(Error::Decode { .. })));
        }
        let zero = parse_ws_message(br#"{"id":8,"code":"0"}"#).unwrap();
        assert_eq!(ws_result(&zero, "read").unwrap(), "null");
        assert!(parse_ws_message(b"42").is_err());
        assert!(parse_ws_message(b"{\"id\":5000000000}").is_err(), "id over uint32");
        assert_eq!(parse_ws_message(b"{}").unwrap().kind, MessageKind::Other);
    }

    #[test]
    fn subscribe_reply_names_the_canonical_selector() {
        let ack = parse_subscribe_result(
            r#"{"stream":"v1.book.d","subs":["X@500-50"],"num_snapshots":["1"],"latest_sequence_number":[100]}"#,
            "X@500",
        )
        .unwrap();
        assert_eq!(
            ack,
            SubscribeAck {
                selector: "X@500-50".into(),
                latest_sequence_number: 100,
                num_snapshots: 1
            }
        );
        assert_eq!(parse_subscribe_result("{}", "X@500").unwrap().selector, "X@500");
    }
}
