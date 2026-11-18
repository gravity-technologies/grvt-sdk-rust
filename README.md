# grvt (Rust SDK, preview)

> **Preview.** This SDK is still being stabilised. Its API can change in any 0.x release, so pin an exact
> version. It becomes stable at 1.0.0.

The Rust SDK for Grvt, in two layers in one crate:

- **Core** (always built): models, EIP-712 signing, request building, reply and stream-frame parsing,
  session state, sequencing, order books and recovery steps. No I/O, no async runtime, no tokio. For
  trading engines that run their own sockets, threads and event loop (mio, io_uring, busy-poll).
- **Client** (feature `client`, on by default): `Grvt`, an async client (tokio) for REST, WebSocket
  streams, WebSocket order entry and login, built on the core. Same method names and behaviour as the
  Python SDK in `sdk/languages/python`.

Everything that grows with the API (models, enums, error codes, RPC/stream descriptors, sign functions,
client methods) is generated from `sdk/contract/contract.json` by `sdk/generator/rust`; the hand-written
runtime in `src/runtime` stays the same size however many APIs exist.

## Features

```toml
grvt = { path = "/path/to/grvt-sdk-rust" }                                   # core + async client
grvt = { path = "/path/to/grvt-sdk-rust", default-features = false }         # core only
```

`client` pulls in tokio, reqwest, tokio-tungstenite, futures, bytes and tracing-subscriber (`GRVT_LOG`).
Without it none of those are built (`cargo tree --no-default-features -e normal`). The core uses
serde, serde_json, rust_decimal, secp256k1, sha3, rand (OS random source), thiserror and tracing.

Minimum Rust: 1.88 (the dependencies need it); the repo builds with the pinned 1.94 toolchain.

## Quickstart

Use the [canonical quickstart](https://developers.grvt.io/guides/quickstart) and
[download its Rust example](https://developers.grvt.io/downloads/examples/rust/examples/quickstart.rs).
The example reads public testnet data by default. Add `--trade` to place, observe, and cancel one testnet order.
Trading requires `GRVT_API_KEY`, `GRVT_PRIVATE_KEY`, and `GRVT_SUB_ACCOUNT_ID`. An order can fill before cancellation.
The example uses current instrument limits and stops after bounded observation attempts.

From this SDK's directory, run:

```sh
cargo run --locked --example quickstart
```

For trading, pass application arguments after Cargo's separator: `cargo run --locked --example quickstart -- --trade`.

Every RPC is a method directly on the client, `client.<method_name>()` (`client.create_order()`), named
exactly as in the contract and as in Python; every stream method ends in `_stream` (`client.trade_stream()`).
The contract's namespace is only a docs category and never appears in your code. Set request fields with setters named like the wire fields, then `.await`. Required fields
are checked before anything is signed or sent. Streams work the same way and resolve to a
`Subscription`, a `futures::Stream<Item = grvt::Result<Feed>>`; dropping it unsubscribes.

Errors are one enum, `grvt::Error`: `Api { code, message, status, rpc, retryable }`, `RateLimit`,
`Transport`, `OrderStateUnknown { rpc, reason, client_order_id }`, `StreamOverflow`, `Signing`,
`Decode`, `Config`, `Closed`. Error code constants are in `grvt::error_codes`.

## Without the client

Every RPC and stream also has a descriptor type in `grvt::ops`, named like the client method in
CamelCase (`ops::CreateOrder`, `ops::OrderbookDeltaStream`). The core functions take it as a type
parameter and return plain data; your code does the sending.

```rust
use grvt::{models as m, ops, signing, Environment, RequestDefaults, RpcOp, SignContext};

let env = Environment::testnet();
let ids = grvt::prepare_request(ops::CreateOrder::SPEC, &mut req, &RequestDefaults { sub_account_id: Some(sub), client_order_ids: None })?;
signing::sign_order_create_order(&mut req, &SignContext::new(&key, &metadata, env.chain_id))?;
let http = grvt::rest_request::<ops::CreateOrder>(&env, &req, Some(&session))?;   // method, url, headers, body
// ... send it ...
let reply = grvt::rest_response::<ops::CreateOrder>(status, &body, &ids);               // or, if the answer is lost:
let err = grvt::transport_failure(ops::CreateOrder::SPEC, sent, "timeout", ids);   // OrderStateUnknown for a write
```

- **Signing** is synchronous and reads metadata only from the `Meta` you pass in. Per signed call:
  `sign_<call>(&mut req, &ctx)`, `needs_<call>(&req)` (the instruments, currencies and main account it
  reads), and for bulk orders `digests_<call>` / `attach_<call>` to sign elsewhere (threads, a KMS).
  `SignContext` holds the signer (`SyncSigner`; `PrivateKeySigner` implements it), metadata, chain id,
  optional clock and nonce source, order checks on/off and `recover_signer`. A wallet signer reads
  `SigningRequest::typed_data()` (eth_signTypedData_v4 JSON). Nonces come from the OS random source.
- **WebSocket**: `ws_request::<Op>(&req, id)` builds the JSON-RPC frame (`ws_request_into` writes it into a
  buffer you keep, and `encode_request_into` the bare request JSON); `parse_ws_message(bytes)` parses
  a frame once into a `WsMessage` (kind, id, stream, selector, sequence numbers, raw payload slice);
  `ws_response::<Op>(&msg, &[])` and `decode_feed::<Stream>(&msg)` decode from it. `subscribe_frame`,
  `parse_subscribe_result`, `RequestTable`, `Sequencer` and `Backoff` cover subscriptions, replies,
  gaps and reconnects.
- **Lite encoding**: call the same functions on `Encoding::Lite` (`Encoding::Lite.rest_request::<Op>(..)`,
  `.ws_request::<Op>(..)`, `.parse_ws_message(..)`, `.subscribe_frame(..)`); `ws_response` and
  `decode_feed` follow the `WsMessage`'s encoding. Use `Environment::ws_url_for(gateway, Encoding::Lite)`.
- **Session**: `api_key_login_request(&env, key)`, then `SessionState::on_login_response(...)`;
  `headers()`, `fresh(now)` and `refresh_due()` say what to send and when to log in again.
- **Recovery**: `resolve_order_request` / `resolve_order_result`, `cancel_countdown_request`,
  `backfill_status`, `PageCursor`, `OrderBook::on_gap`.

`examples/order_and_subscribe.rs` runs all of it against a fake transport:
`cargo run --example order_and_subscribe --no-default-features`.

## Generated source

The public source repo includes the generated code and shared test fixtures. For development in the platform
monorepo, run `make sdk-gen-rust` from its root before building this SDK. Generated source comes from the
Grvt API contract. Send proposed changes to the Grvt maintainers so they can update the monorepo.

## Layout

```
src/lib.rs              public names: core functions and types, plus Grvt, Config, ... with `client`
src/generated/          GENERATED (do not edit): models, lite (lite codecs), enums, errors, specs, ops, signing (core);
                        api, client (feature `client`)
src/runtime/            hand-written runtime, same size however many APIs exist
  core (no I/O):
  request.rs            Request trait, prepare_request, REST/WS request builders, WsMessage parsing,
                        reply/feed decoding, transport_failure, may_retry
  session.rs            api-key login request and reply -> SessionState (cookie, account id, expiry)
  protocol.rs           request table, sequencer, backoff, GapRange
  signing/              SignContext, SigningRequest, SyncSigner, PrivateKeySigner, typed data,
                        wire transforms + encoder writers, order checks
  codec.rs, spec.rs     wire rules; RpcSpec/StreamSpec and the RpcOp/StreamOp/FeedFrame traits
  lite.rs               Encoding and the lite codec traits
  env.rs, clock.rs, error.rs, http.rs (HttpResponse), log.rs (redaction)
  client (feature `client`):
  client.rs             Runtime, Config, RpcCall / StreamCall builders, metadata cache, client signing
  ws.rs                 one managed socket per gateway; Subscription
  rest.rs, http.rs      REST calls with read retries; HTTP transport plug-in (reqwest + rustls)
  auth.rs               shared login, re-login at 80% of lifetime
  hooks.rs, testing.rs  before_send / on_event; FakeHttp
RUNTIME_API.md          the contract between generator (sdk/generator/rust) and runtime
examples/               quickstart (client), order_and_subscribe (core, fake transport)
tests/                  signing case replay (core and client), core requests, fake-gateway WS/REST tests,
                        one testnet smoke test
benches/hot_path.rs     sign / encode / decode / stream-path timings
benches/lite.rs         full vs lite decode and encode timings
```

## Run the tests

```
cargo test                                                         # everything offline
cargo test --no-default-features                                   # the core alone
GRVT_SMOKE=1 cargo test --test smoke_testnet -- --ignored --nocapture   # read-only testnet check
cargo bench --bench hot_path
cargo bench --bench lite                                           # full vs lite
```

## How the main pieces behave

Same as Python (see `sdk/languages/python/README.md`):

- **Order transport** is per client (`OrderTransport::Rest` default, or `Ws`). Writes are never
  retried. If a write may have reached the exchange but no answer came back (timeout, socket drop),
  the call returns `Error::OrderStateUnknown` with the client order id; look the order up, don't
  resend.
- **Reads** retry up to 3 times with full-jitter backoff on retryable errors.
- **Order checks**: before signing, each leg is checked against its instrument's tick, size step,
  `min_size` and `min_notional`. A miss returns `Error::OrderOutOfBounds` with the nearest valid values;
  nothing is sent and nothing is rounded. If the exchange still rejects an order for these limits, the
  instrument's cached metadata is dropped so the next order refetches it. `.check_orders(false)` on the
  `Config` turns the check off and leaves the limits to the exchange.
- **Streams** use global sequence numbers. The snapshot frame has `sequence_number` 0, and the
  first update's `prev_sequence_number` is at or below the subscribe reply's `latest_sequence_number`. The
  first frame after a (re)subscribe reply is accepted
  when its `prev_sequence_number` is 0 or not greater than the latest sequence number known then
  (testnet sends the old sequence number there). After that, `prev_sequence_number` must equal the
  last sequence number seen; otherwise it is a gap: the runtime re-subscribes the same selector and
  drops frames until the new snapshot. No snapshot within 5s -> reconnect. On reconnect every
  subscription is re-sent.
- **Backpressure**: the read loop never waits for a consumer; each subscription has a bounded queue.
  Only snapshot streams (every frame complete, `*.s`) drop the oldest frame when the consumer falls
  behind. Every other stream (deltas, events, private) yields `Error::StreamOverflow` and re-syncs.
- **Encoding**: by default (`Encoding::Lite`) the client sends and reads short JSON field names
  (`/lite/v1/...`, `/ws/lite`) on every gateway call and stream; `.encoding(Encoding::Full)` switches to
  full names. Order book
  frames are about 30% smaller. Models, `before_send`, signing and errors keep full names; edge login
  and `GET /time` stay full; raw `Event::Frame` bytes and debug wire logs show the lite names. A custom
  `Environment` needs WS URLs ending in `/ws/full` for lite (the lite URL swaps that path). Core
  functions: the same names on `Encoding::Lite`.
- **Hooks**: `before_send` (sync closure, runs before signing, may edit the request as JSON) and
  `on_event` (typed `Event` enum). Unset hooks cost one `Option` check.
- **Logging**: `tracing`, target `grvt`. `GRVT_LOG=debug` installs a stderr subscriber if the app has
  none. Cookie, api key, private keys and r/s are redacted.

## Crate choices

| need | crate | why |
|---|---|---|
| async (client) | tokio 1 | the default Rust async runtime; what reqwest and tungstenite use |
| HTTP (client) | reqwest 0.13 (rustls) | keep-alive pool, timeouts, TLS without OpenSSL |
| TLS (client) | rustls on aws-lc-rs, OS trust store | REST and WebSocket trust the same roots (the OS store, so corporate CAs work); aws-lc-sys needs a C compiler to build, and cmake for some cross targets |
| WebSocket (client) | tokio-tungstenite 0.30 (rustls) | mature, handles ping/pong and TLS; fastwebsockets is faster at the frame level but needs its own HTTP upgrade and TLS glue, which is not where the time goes |
| secp256k1 | secp256k1 0.31 (libsecp256k1) | 11.8 µs vs 15.9 µs for k256 0.14 per recoverable sign (bench). Not 0.33: it re-randomizes its global context on every signature, which doubles the cost (23.5 µs); `PrivateKeySigner` blinds its own context once instead |
| keccak | sha3 0.12 | 360 ns vs 457 ns for tiny-keccak on 288 B (bench) |
| random | rand 0.10 | nonces straight from the OS source (`SysRng`); jitter from a seeded `StdRng` |
| decimals | rust_decimal | exact scaling and over-precision checks when signing |
| errors / logs | thiserror 2 / tracing | the usual choices |

## Draft notes (shortcuts and TODOs)

- **Builders instead of positional args.** Requests have up to 11 required fields (transfer), so RPC
  methods return a fluent builder (the aws-sdk-rust style) instead of taking arguments. Required
  fields are checked at send time, not at compile time.
- **Exact values:** decimal fields use `rust_decimal::Decimal`. Decoding rejects values outside its
  precision or range instead of rounding. Timestamps use `grvt::Timestamp` over signed `i64` Unix
  nanoseconds, with explicit `from_nanoseconds()` and `nanoseconds()` conversions.
- **Required fields use native values.** Optional and nullable fields use `Option`. Decoding rejects
  missing required keys, including nullable keys. Signed calls use `Input` models for SDK-filled fields.
  Only contract-declared partial streams use `Partial` payload models.
- **`before_send` is a sync closure** that edits the request as JSON. An async hook would need a
  boxed future per call; not needed yet.
- **Session rotation on WS is simplified** (break-before-make, same as Python): after a re-login a
  socket waits for no writes in flight, then reconnects with the new cookie.
- **Frame-size check exists but is off** (`Environment::ws_max_frame_bytes` is `None` for named envs);
  when set, an oversized frame is `Error::FrameTooLarge` and nothing is sent. REST fallback is a TODO.
- **Book deltas are never conflated.** The local `OrderBook` helper merges levels.
  Queue overflow invalidates continuity. See [rebuild a book after a gap](https://developers.grvt.io/guides/recovery#rebuild-a-book-after-a-gap).
- **Private-stream resume** is not possible until the server supports it; callers backfill via REST.
- **Decode speed**: serde_json decodes a 10+10 level book delta in about 1.7 µs (decimals are parsed
  in place, 9 allocations: the strings and lists the model owns); the client's whole stream path for
  that frame (parse once, route, sequence, decode the payload) takes about 2.9 µs, over a third of it the
  routing parse skipping over the payload. Reusing frame objects (C++ `decode_into`) or simd-json are
  the next step if it matters.
- **Signing speed**: signing a `create_order` takes about 13.4 µs, 11.7 µs of it in secp256k1; the
  SDK's part (order checks, hashing, r/s/v) is about 1.7 µs, mostly keccak, with 6 allocations (the
  r, s and signer strings the request keeps, and two short lists).
- **Signing scope**: every signed call in the contract: Order / OrderWithBuilderFee (also each order
  of `bulk_orders`, signed on its own), Transfer V1-V3, Withdrawal V1-V2, WalletLogin, vault invest,
  redeem and burn, position margin and config, derisk ratio, qualified-maker funding rates. Algo orders
  and session keys are not bound in the contract yet.
- **Signing without sending** with the client: `client.sign_<call>(req).await` (e.g.
  `client.sign_order_create_order(req)`) signs the way the RPC method does and returns the request.
- **Authentication belongs to the session runtime.** Set `Config::api_key` on the client to log in.
  Raw edge login routes are not generated client methods; wallet session login is not supported.
- **Client order ids** use the wall clock (`TimeCounterClientOrderId`), not the configured `Clock`.
- No Bazel BUILD files for the crate or the generator.

## Timeouts

`Config::call_timeout(Duration)` defaults to 30 seconds for the whole RPC, including authentication, metadata, signing, retries, and the response.
`Config::http_attempt_timeout(Duration)` caps each HTTP attempt at 10 seconds by default, within the remaining total limit.
Set `RpcCall::timeout(Duration)` on one call to override its total limit.
Replace the old `request_timeout` configuration with `http_attempt_timeout` and `with_timeout` with `timeout`.

## Uncertain orders and stream gaps

Use the [recovery helpers and gap ranges](https://developers.grvt.io/guides/recovery) to look up uncertain orders and detect lost stream continuity.
A missing order stays unknown. The helpers never resend an order.

Bulk order models and setters use `order_ids` and `client_order_ids`. Serde preserves the server's
`order_i_ds` and `client_order_i_ds` JSON keys.

## Pagination

Use `pages` to follow cursors without fetching ahead. Keep filters fixed in the callback.

```rust
use futures::{pin_mut, StreamExt};
use grvt::pages;

let history = pages(|cursor| {
    let mut call = client.trade_history().instrument("BTC_USDT_Perp");
    if let Some(cursor) = cursor { call = call.cursor(cursor); }
    async move { call.await }
}, None);
pin_mut!(history);
while let Some(page) = history.next().await {
    for trade in page?.result.unwrap_or_default() { println!("{trade:?}"); }
}
```

An empty page can still have a next cursor. Dropping the stream stops further calls.
A repeated cursor returns `Error::PaginationCycle`. Fetch errors pass through without extra retries.
Cursors do not guarantee a stable snapshot while records change.

## Default sub-account

Set `Config::testnet().sub_account_id(123)` to fill omitted request and stream account IDs.
Explicit IDs take precedence. For a complete request supplied through `with_request`, a zero account means omitted.
`OrderInput` uses `None` for omitted. Bulk orders first inherit the request account, then the client default.
Transfer source and destination IDs stay explicit. Defaults are applied before signing.
Without a default, calls that need an account still require one.

## Constructing models

Generated structs and enums are non-exhaustive.
This lets a new SDK add fields or enum variants without breaking existing construction and match expressions.
Construct request and nested input models with `Type::new(required_fields...)`.
Use typed `with_*` setters for optional values. Set an optional or nullable field to `None` directly to clear it.
`Default` initializes storage for response fixtures and does not prove that a request is valid.
Required fields remain native values, and decoding rejects absent required fields.

For RPC requests, prefer the client's generated call builder.
It checks required setters before sending.
For enum matches, include a wildcard arm even when handling `Unknown(String)`.
Known new variants and unknown wire strings are different cases.

## Recovery helpers

The SDK exposes lookup-only order resolution, history backfill with unproven continuity,
and explicit cancellation-countdown refresh.
Session rotation reports that authenticated WebSockets must reconnect.
See the [recovery recipes](https://developers.grvt.io/guides/recovery), including [rebuilding a local order book](https://developers.grvt.io/guides/recovery#rebuild-a-book-after-a-gap), before restoring trading after a gap.
