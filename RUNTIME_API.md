# Rust runtime API (generator target)

Generated code in `src/generated/` talks to the hand-written runtime (`src/runtime/`) only through
the names on this page. If the generator needs something not listed here, add it to the runtime and
to this page; never reach into private fields. The one reference the other way: the core request
functions classify errors with the generated `errors::TABLE` (`crate::error_codes::TABLE`).

Two layers. The core (always built, no I/O) holds models, enums, errors, specs, `ops.rs` and
`signing.rs`. The client (feature `client`) holds `api.rs` and `client.rs`; `mod.rs` gates both with
`#[cfg(feature = "client")]`. Generated core code must not name anything from the client
(`Runtime`, `RpcCall`, `StreamCall`, futures, tokio).

The generator is `sdk/generator/rust` (`make sdk-gen-rust`, or `make sdk-gen-code` for every SDK and the docs). Its
output in `src/generated/` (not committed) is the reference for everything below. Naming and write rules shared
with the other generators live in `sdk/generator/naming`.

## 1. Models (one struct per contract type)

```rust
/// One-line description.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OrderLeg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrument: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "c::opt_u64")]
    pub sub_account_id: Option<u64>,
}
```

- Public field names follow contract aliases; a Rust keyword becomes `r#type`. Required fields use
  native values. Optional and nullable fields use `Option<T>`. Decoding requires nullable keys too.
  Signed calls use `Input` models. Only declared partial streams use `Partial` payload models.
- Wire type -> Rust type (from the contract's `type`):

  | wire | Rust | serde |
  |---|---|---|
  | decimal string (`"65000.1"`) | `rust_decimal::Decimal` | exact; out-of-range values are rejected |
  | uint64 / int64 (JSON string) | `u64` / `i64` | `codec::opt_u64` / `opt_i64`: written as a string, read from string, number or 0x hex |
  | list of uint64 | `Vec<u64>` | `codec::opt_vec_u64` |
  | uint128 / uint256 (hex) | `String` | |
  | uint8/16/32, int8/16/32 | `u8`... `i32` | `codec::opt_num`: written as a number, also read from a quoted number |
  | enum | `enums::X` | `wire_enum!`: the wire string; unknown values decode to `X::Unknown(String)` |
  | bool / string / any / list / nested type | `bool` / `String` / `serde_json::Value` / `Vec<T>` / `T` (`Box<T>` if recursive) | |

- Unknown fields are ignored on decode.

### Lite codecs (`lite.rs`)

The lite encoding renames every field per type (`types[].fields[].lite`); models keep their full names.
Each model the SDK sends or receives gets a private serde mirror that encodes from and decodes into the
model itself (serde `remote`), so a lite decode builds the model directly, at the cost of a full one:

```rust
#[derive(Serialize, Deserialize)]            // only the directions used: Serialize for request models,
#[serde(remote = "m::OrderbookLevels")]      // Deserialize for response and frame models
struct OrderbookLevels {
    #[serde(rename = "i")]
    instrument: String,
    #[serde(rename = "b", with = "l::nested")]   // fields holding models use their lite codecs
    bids: Option<Vec<m::OrderbookLevel>>,
    // ...
}
crate::lite_codec!(m::OrderbookLevels, OrderbookLevels, decode);   // LiteDecode / LiteEncode impls
crate::full_codec!(m::EdgeApiKeyLoginRequest);                   // edge models: no lite form
```

Runtime names: `runtime::lite::{Encoding, LiteEncode, LiteDecode, AsLite, FromLite, nested}`, the
`lite_codec!` and `full_codec!` macros. `Request` requires `LiteEncode`, `RpcOp::Response` and frame
payloads `LiteDecode`. `lite.rs` also emits `CHECKS` (tests only): `runtime/lite_test.rs` compares every
codec with the contract's names.

## 2. RPCs (one `RpcSpec` static + one method + one `Request` impl + setters)

```rust
pub static ORDER_CREATE_ORDER: RpcSpec = RpcSpec {
    method: "v1/create_order",      // JSON-RPC method == REST route tail
    gateway: Gateway::Trading,      // MarketData | Trading | Edge
    path: "/full/v1/create_order",  // REST path (edge routes use their own path)
    lite_path: "/lite/v1/create_order", // REST path in lite (transport.lite.http_path_template); "" for edge
    auth: true,                     // needs the session cookie
    write: true,                    // never retried; ambiguous failure -> Error::OrderStateUnknown
    order_entry: true,              // goes over the trading WS when OrderTransport::Ws
    refresh_metadata_on: &[2062, 2064, 2065, 2066, 2067], // order calls only (else &[]): on these
                                    // codes, drop the request's legs[].instrument from the metadata cache
};

// ops.rs (core): one descriptor per RPC and edge route, named like the client method in CamelCase.
pub struct CreateOrder;
impl RpcOp for CreateOrder {
    type Request = m::ApiCreateOrderRequestInput;
    type Response = m::ApiCreateOrderResponse;
    const SPEC: &'static RpcSpec = &specs::ORDER_CREATE_ORDER;
}

// api.rs (client)
impl Grvt {
    pub fn create_order(&self) -> RpcCall<'_, m::ApiCreateOrderRequestInput, m::ApiCreateOrderResponse> {
        RpcCall::new(self.runtime(), &specs::ORDER_CREATE_ORDER, Some(&signing::SIGNING_ORDER_CREATE_ORDER))
    }
}

// ops.rs (core): the Request trait (runtime::request) for every request type.
impl Request for m::ApiCreateOrderRequestInput {
    fn required_fields() -> &'static [&'static str] { &["order"] }
    fn fill_client_order_ids(&mut self, ids: &dyn ClientOrderIdProvider) -> Vec<u64> { /* order writes only; one per order */ }
}

impl<Resp> RpcCall<'_, m::ApiCreateOrderRequestInput, Resp> {
    pub fn order(mut self, v: m::OrderInput) -> Self { self.req.order = v; self.provided.as_mut().unwrap().insert("order"); self }   // one per field
}
```

`RpcCall` is a fluent builder (`IntoFuture`): setters named like the wire fields, then `.await`.
`with_request(req)` replaces the whole request; `timeout(d)` overrides the timeout. Setter
names must not clash with those methods (the generator fails if they do). String setters take
`impl Into<String>`.

`RpcCall` checks that each required setter ran before calling the runtime. `with_request` supplies a complete model.
`Runtime::call(spec, req, sign, timeout)` does, in order: `prepare_request` (default account,
`missing_field` check, `fill_client_order_ids`) -> `before_send` hook (only if set: request -> JSON ->
request; adding, removing or reordering orders, or changing their client order IDs, is an `Error::Config` and nothing is sent) ->
`Runtime::sign` -> encode (full JSON for the stale instruments; the wire body in the client's encoding) -> REST (`rest_request_body`, `rest_response_body`) or WS
(`jsonrpc_frame`, `parse_ws_message`, `ws_result`) -> `decode_response`. Each of those is an
`Encoding` method; `RpcSpec::encoding(e)` and `path_for(e)` keep edge routes full.

The core request functions (`runtime::request`) take an `RpcOp` as a type parameter:
`rest_request::<O>`, `rest_response::<O>`, `ws_request::<O>`, `ws_response::<O>`, plus
`ws_request_into::<O>` and `encode_request_into` (the same bytes into a caller-kept `Vec<u8>`, no
allocation once it has grown), `prepare_request(spec, ..)`, `transport_failure(spec, sent, why, client_order_ids)`, `may_retry`,
`stale_instruments`. For REST the whole body is the response; over WS the JSON-RPC
`result` is the same struct. The free functions are full; the same names on `Encoding::Lite`
(`Encoding::Lite.rest_request::<O>(..)`, `.parse_ws_message(..)`, `.subscribe_frame(..)`, ...) speak lite, and a
`WsMessage` carries its `encoding` so `ws_response` and `decode_feed` need no argument. Errors are `Error::{Api, RateLimit, Transport, OrderStateUnknown,
OrderOutOfBounds, ...}`.

## 3. Streams (one `StreamSpec` static + one method + a marker type + setters)

```rust
pub static STREAM_ORDERBOOK_ORDERBOOK_DELTA: StreamSpec = StreamSpec {
    stream: "v1.book.d", gateway: Gateway::MarketData, auth: false,
    snapshot_feed: false,  // true when every frame is complete (e.g. v1.ticker.s): gaps need no resync
    conflate: false,       // true: old items may be dropped for a slow consumer; false: StreamOverflow
};

pub fn orderbook_delta_stream(&self) -> StreamCall<'_, m::WSOrderbookLevelsFeedDataV1, streams::OrderbookOrderbookDelta> {
    StreamCall::new(self.runtime(), &specs::STREAM_ORDERBOOK_ORDERBOOK_DELTA, 1, 2, &[(0, "instrument")])
}

impl<F> StreamCall<'_, F, streams::OrderbookOrderbookDelta> {
    pub fn instrument(mut self, v: impl Into<String>) -> Self { self.primary[0] = Some(v.into()); self }
    pub fn rate(mut self, v: u32) -> Self { self.secondary[0] = Some(v.to_string()); self }
}
```

`StreamCall::new(rt, spec, n_primary, n_secondary, required)` where `required` lists
`(primary index, name)`. Awaiting it checks required fields, logs in if `auth`, subscribes and waits
for the ack, and returns `Subscription<F>`: a `futures::Stream<Item = Result<F>>`. Dropping it
unsubscribes.

ops.rs (core) has one `StreamOp` per stream and one `FeedFrame` per frame model:

```rust
pub struct OrderbookDeltaStream;
impl StreamOp for OrderbookDeltaStream {
    type Selector = m::WSOrderbookLevelsFeedSelectorV1;
    type Feed = m::WSOrderbookLevelsFeedDataV1;
    const SPEC: &'static StreamSpec = &specs::STREAM_ORDERBOOK_ORDERBOOK_DELTA;
    fn selector(s: &m::WSOrderbookLevelsFeedSelectorV1) -> String { w::selector(&[..], &[..]) }
}

impl FeedFrame for m::WSOrderbookLevelsFeedDataV1 {
    fn from_message(msg: &WsMessage<'_>) -> serde_json::Result<Self> {
        Ok(Self { stream: .., selector: .., sequence_number: .., feed: msg.feed_as()?, prev_sequence_number: .. })
    }
}
```

`FeedFrame` builds the frame from the one parse (`parse_ws_message`): stream, selector and sequence
numbers come from `WsMessage`, only `feed` is decoded (`WsMessage::feed_as`, or `feed_opt_as` for an optional
feed; a missing required field is `WsMessage::missing(name)`). A frame model with any other field decodes
`msg.text` whole (`WsMessage::whole_as`). All three decode in `msg.encoding`. The client's
read loop parses each frame once, routes it, and queues the frame with the position of its payload;
`Subscription<F: FeedFrame>` decodes through `FeedFrame` on the consumer's side. The selector is `p1-p2@s1-s2` over the fields that are set
(`wire::selector`). The marker type exists so two streams with the same feed type get separate
setters. Every stream method gets a `_stream` suffix (`naming.StreamSuffix`), so it never shares a name
with an RPC.

## 4. Signed types (one `StructType` static + message builders + one sign function per signed call)

```rust
const EIP712_ORDER_TYPES: &[TypeDef] = &[
    ("Order", &[("subAccountID", "uint64"), /* ... */ ("legs", "OrderLeg[]"), ("nonce", "uint32"), ("expiration", "int64")]),
    ("OrderLeg", &[("assetID", "uint256"), /* ... */]),
];
pub static EIP712_ORDER: LazyLock<StructType> =
    LazyLock::new(|| StructType::new("Order", EIP712_ORDER_TYPES, "0xb4b857ba...").expect("generated EIP-712 type"));
```

`StructType::new` compiles the encoders and checks the type hash. Messages are positional:
`Value::Struct(vec![...])` in field order, built from the typed request (`message_<type>(d, meta)`),
and hashed by the generated `hash_<type>(d, meta)` (below).

Per signed call (`id` = the call id in snake case, e.g. `order_create_order`), all core:

```rust
fn sig_<id>(req: &mut View) -> &mut m::SignatureInput;                 // the signature object
fn prepare_one_<id>(req: &mut View, ctx: &PrepareContext<'_>, check: bool) -> Result<SigningRequest<'static>>;
    // saves the signature object, calls fill_and_hash_<id>, puts it back on an error; fill_and_hash_<id>:
    // w::prepare_signature(ctx, signer, nonce, expiration, chain_id, TTL)? -> chain id of the domain
    // -> check_order(&ORDER_CHECKS, Some(&d.order), meta)? when check && ctx.check_orders
    // -> variant rule picks (StructType, hash_<type>(d, meta)?) -> w::signing_request(chain_id, st, hash)
fn typed_message_one_<id>(d: &View, meta: &Meta) -> Result<Value>;     // same variant rule, message_<type>
fn commit_one_<id>(req: &mut View, s: Signature);                       // w::write_signature(...)
fn prepared_one_<id>(sig: Option<&m::SignatureInput>) -> bool;          // w::is_prepared(...)
fn count_<id>, prepare_<id>, prepared_<id>, typed_message_<id>, commit_<id>  // over the whole request
    // prepare_<id> of a list is all or nothing: on an error every element's signature object is restored
pub fn needs_<id>(req: &Req) -> SigningNeeds;                           // instruments, currencies, main account
pub static SIGNING_<ID>: SignSpec<Req> = SignSpec { call, needs, count, prepare, prepared, message, commit };
pub fn sign_<id>(req: &mut Req, ctx: &SignContext<'_>) -> Result<()> { crate::runtime::signing::sign(req, ctx, &SIGNING_<ID>) }
// calls that sign every element of a list (bulk orders) also:
pub fn digests_<id>(req: &mut Req, ctx: &SignContext<'_>) -> Result<Vec<SigningRequest<'static>>>;  // signing::digests
pub fn attach_<id>(req: &mut Req, ctx: &SignContext<'_>, signatures: &[Signature]) -> Result<()>;   // signing::attach
```

`View` is the request type itself, or for a per-element binding the plain binding's request type
holding one element (`ApiCreateOrderRequestInput { order }`): elements are moved into it and back
(`std::mem::take`), cloned only for a typed-data signer's message.

The runtime's generic `signing::sign`, `digests` and `attach` drive a `SignSpec`: `sign` prepares,
signs each `SigningRequest` with `ctx.signer` (a `SyncSigner`; `SigningRequest::typed_data()` builds
eth_signTypedData_v4 JSON on demand through `message`), checks each signature's shape (and recovers it
when `ctx.recover_signer`), then commits. `attach` checks the count, that every signature object is
prepared, re-hashes without order checks and always recovers every signature against its element's
digest. `prepare` takes a `PrepareContext` (signer address, metadata, chain id, clock, nonce source, order-check
switch; `SignContext::prepare_context` builds one), so the client needs no core signer. The client's
`Runtime::sign` (crate-private; users call the generated `Grvt::sign_<id>(req)`) drives the same
`SignSpec` with its async `Signer`: `prepare`, then
`sign_prehash(digest)` or `sign_typed(domain, st, message)` (recovered against the digest), then
`commit`.

Contract transform -> generated expression (`crate::runtime::signing::wire` as `w`):

| transform | code |
|---|---|
| `copy`, `sig.nonce`, `sig.expiration` | `w::copy(path, "<eip type>", "<where>", default)?` (`default`: `Some(Scalar::Bool(false))`, `Some(Scalar::Str("0"))` or `None`) |
| `address` | `w::address(path, where)?` (absent = zero address) |
| `enumCode` | `w::enum_code(path, where)?` (absent = 0; an `Unknown` value is an error) |
| `scaled` fixed | `w::scaled(path, 9, where, default)?` (rejects over-precision, never rounds) |
| `scaled` instrument_base_decimals | `w::scaled(path, meta.instrument(inst_path)?.base_decimals, ...)` |
| `scaled` instrument_quote_decimals | `w::scaled(path, meta.instrument(inst_path)?.quote_decimals, ...)` |
| `scaled` currency_balance_decimals | `w::scaled(path, meta.currency(cur_path)?.balance_decimals, ...)` |
| `assetKey` | `meta.asset_key(path)?` |
| `currencyId` | `meta.currency_id(path)?` |
| `mainAccount` | `meta.main_account()?` (`needs_<id>` sets `main_account`; the client loads it once) |
| `each` | `Value::List(w::each(path).map(\|e\| message_<type>_<nested>(e, meta)).collect()?)` |

`ORDER_CHECKS: OrderChecks` is the contract's `order_checks` (`scales` as `Scale::Fixed(n)` or
`Scale::BaseDecimals`, `rules` as `OrderCheckRule`). `check_order` returns `Error::OrderOutOfBounds` for
the first leg outside its instrument's limits (read from `InstrumentMeta::limits`, which
`instrument_meta` fills) and `Error::Signing` for a malformed value; it never changes the order.

Metadata converters (core): `instrument_meta(&m::Instrument) -> InstrumentMeta`,
`currency_meta(&m::CurrencyDetail) -> CurrencyMeta`, `main_account(&response) -> Result<String>`. The
client's loaders (client.rs, one RPC each) call them and fill `MetadataLoaders`.

Variant rule kinds the generator knows: `builder`, `wallet_types`, `currency_width`, and a single
type. An unknown kind fails generation.

The SDK signs only Grvt's own messages; it has no generic EIP-712 hashing and no public way to build
custom struct types (`StructType::new` is crate-only; the generated `EIP712_*` statics are the only
instances).

Hashing is generated: `hash_<type>(d, meta) -> Result<[u8; 32]>` fills one `[u8; 32 * (fields + 1)]`
buffer (type hash constant first) with `w::put_uint` / `put_int` / `put_bool` / `put_address`, hashes
array items one by one into `w::ListHasher`, and allocates nothing unless a value is rejected (then it
returns the error building the message would). The digest is `w::signing_request` /
`signing::digest(chain_id, struct_hash)` (domain separator cached per thread for the last chain id).

Signatures from any signer must be low-s (s at most half the curve order), v 27 or 28, and have a
signer field that is empty or equal to the signer's address, else `Error::Signing`; a typed-data signer
in the client and every signature in `attach_*` must also recover to that address. Some KMS backends
(AWS KMS, for one) return a high s: normalize it (s = n - s, flip v). A client `Signer` that wraps
another one must forward `sign_prehash` as well as `sign_typed`.

A generic encoder exists only in `tests/reference`, as the reference `tests/encoders.rs` compares every
generated encoder against on random requests.

## 5. Errors

```rust
pub const SIGNATURE_EXPIRED: u32 = 2001;
pub static TABLE: ErrorTable = ErrorTable(&[ErrorInfo { code: 1000, name: "Unauthorized", status: 401, message: "...", retryable: false }, /* sorted by code */]);
```

The runtime reads `TABLE` directly (core request functions and client alike) for the retry rule
(`rate_limit_code`, a code the table marks retryable, or a status in `read_retry_statuses`; both keys live in the generated behavior module).

## 6. Enums

One `enum` per contract enum with an `Unknown(String)` variant, `ALL`, `as_str()`, `from_wire()`, a
`WireEnum` impl (`code()` = the signed integer) and `crate::wire_enum!(Name)` for Display, FromStr,
serde and `ToScalar`.

## 7. Client shell

```rust
pub struct Grvt { rt: Runtime }
impl Grvt {
    pub fn new(config: Config) -> Result<Grvt> { /* config.env.validate()?, then Runtime::new(config, &METADATA) */ }
    // pub async fn sign_<id>(&self, req) -> Result<Req>: one per signed call, self.rt.sign(req, &signing::SIGNING_<ID>)
    // runtime(), close()
}
static METADATA: MetadataLoaders = MetadataLoaders { instrument: load_instrument, currency: load_currency, main_account: load_main_account };
// api.rs adds `impl Grvt { ... }` with one method per RPC and stream (client.create_order()); the
// contract namespace is not part of the name.
```

`Config` (builder): `env`, `api_key`, `signer`, `order_transport` (`Rest` | `Ws`), `encoding` (`Lite` by
default | `Full`; lite WS URLs are the env's with `specs::FULL_WS_PATH` swapped for `specs::LITE_WS_PATH`), `check_orders`, `clock`,
`nonces`, `client_order_ids`, `http` (HTTP plug-in), `before_send`, `on_event`, `call_timeout`, `http_attempt_timeout`,
`ws_request_timeout`, `resync_timeout`, `idle_timeout`, `queue_size`.

## Call deadlines

`Config::call_timeout` bounds the whole RPC: preparation, signing metadata, login,
retry delays and the reply. `RpcCall::timeout` overrides that total for one call.
`Config::http_attempt_timeout` limits each HTTP attempt, including login, and
`ws_request_timeout` caps a WS reply or subscribe. Defaults come from
`src/generated/behavior.rs` (built from `contract/behavior.json`). Durations must be positive, and
`idle_timeout` at least `WATCHDOG_INTERVAL`.

Applications that supply their own transport pass the request's client order IDs (what `prepare_request` returns) to `rest_response` or `ws_response`
(an empty slice for reads and writes that create no orders), so an unknown outcome carries them.

A returned deadline error is unsent during preparation. After dispatch may have
started, a write returns `OrderStateUnknown` with its client order IDs and the
original transport error as its source. Dropping a Rust future cannot return an
error: reconcile a write if dispatch may already have started. Dropping the call
removes its pending WS entry and prevents a queued frame from starting; it never
resends a write.

Cancelling a metadata or login initializer can restart that shared read for a
surviving waiter; it does not poison the cache or fail that waiter. Blocking
synchronous hooks cannot be interrupted. The runtime checks its monotonic deadline
again before dispatch, so preparation that returns late cannot submit a write.

Custom HTTP transports must honor attempt timeouts and future-drop cancellation.
They must not detach queued writes that can start after their future is dropped.
The SDK cannot enforce cancellation inside a transport plug-in; a send that already
started remains uncertain.

## HTTP redirects and retries

The SDK never follows a redirect or retries a write, and a custom HTTP transport must not either: the rule and
why are in [What the SDK retries](https://developers.grvt.io/guides/errors-and-retries#what-the-sdk-retries).

Use `ReqwestTransport::with_builder` to configure a reqwest transport.
It disables redirect following and reqwest protocol retries after applying the supplied builder settings.
