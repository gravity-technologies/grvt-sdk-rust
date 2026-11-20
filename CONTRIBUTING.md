# Contributing to the Rust SDK

This page is for people who change the SDK itself. To use it, see [README.md](README.md).

## How the source is made

Everything that grows with the API (models, enums, error codes, RPC and stream descriptors, sign functions,
client methods) is generated from the Grvt API contract (`sdk/contract/contract.json` in the platform monorepo)
by the generator in `sdk/generator/rust`. The hand-written runtime in `src/runtime` stays the same size however
many APIs exist. [RUNTIME_API.md](RUNTIME_API.md) is the contract between the generator and the runtime.

- The public source repo includes the generated code and the shared test fixtures.
- In the platform monorepo, run `make sdk-gen-rust` from the repo root before building. `src/generated` and
  `CHANGELOG.md` are git-ignored build output, which is why `Cargo.toml` lists the packaged files explicitly.
- Don't edit generated code. Send proposed changes to the Grvt maintainers so they can update the monorepo.
- There are no Bazel BUILD files for the crate or the generator.

## Build from source

1. Install Rust 1.88 or newer (the minimum the dependencies need; the repo pins 1.94 in `rust-toolchain.toml`)
   and the repository's Go toolchain, which runs the generator.
2. From the repository root, generate the SDK: `make sdk-gen-rust`.
3. From your application directory, add the crate by path, plus its async dependencies and `rust_decimal`:

```sh
cargo add grvt --path /absolute/path/to/platform/sdk/languages/rust
cargo add tokio --features full
cargo add futures
cargo add rust_decimal
```

For the core only, add `--no-default-features` to the first `cargo add` and skip `tokio` and `futures`.

## Layout

```
src/lib.rs              public names: core functions and types, plus Grvt, Config, ... with `client`
src/generated/          GENERATED (do not edit): models, lite (lite codecs), enums, errors, specs, ops, signing (core);
                        api, client (feature `client`)
src/runtime/            hand-written runtime
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
src/*.rs                recovery helpers: orderbook, pagination, backfill, countdown, recovery
examples/               quickstart (client), order_and_subscribe (core, fake transport)
tests/                  signing case replay (core and client), core requests, fake-gateway WS/REST tests,
                        one testnet smoke test
benches/hot_path.rs     sign / encode / decode / stream-path timings
benches/lite.rs         full vs lite decode and encode timings
```

## Run the tests

```sh
cargo test                                                              # everything offline
cargo test --no-default-features                                        # the core alone
GRVT_SMOKE=1 cargo test --test smoke_testnet -- --ignored --nocapture   # read-only testnet check
cargo run --example order_and_subscribe --no-default-features           # core example, fake transport
```

The tests and benches read the shared contract and scenarios from outside the crate (`tests/shared`), so they
are not in the published package. `tests/scenarios.rs` runs each shared scenario as its own test, with its own
`main`.

## Benchmarks

```sh
cargo bench --bench hot_path
cargo bench --bench lite        # full vs lite encoding
```

Both are plain loops (no criterion): a warm-up of a tenth of the iterations, then the median of 7 runs.
`hot_path` also prints heap allocations and bytes per op through a counting global allocator. The instrument in
the order benches carries real limits, so the order checks run in full. The machine behind the numbers below
was not recorded; record it when you update them.

`hot_path` measures signing a 2-leg `OrderInput` (core and client), order hashing (generated encoder vs the
reference encoder), secp256k1 vs k256 and sha3 vs tiny-keccak, encoding a signed `create_order` (fresh `Vec`
and kept-buffer `_into` forms), decoding a book delta, and the client's stream path for one 10+10 book frame.
`lite` measures full vs lite for book snapshot frames (10 and 100 levels a side) through the stream path, a
200-instrument `get_all_instruments` response, and encoding a `create_order` WS request.

Last recorded results:

- **Decode**: serde_json decodes a 10+10 level book delta in about 1.7 µs. Decimals are parsed in place; the
  9 allocations are the strings and lists the model owns.
- **Stream path**: about 2.9 µs for that frame (parse once, route, sequence, decode the payload). Over a third
  of it is the routing parse skipping over the payload. Next steps if it matters: reuse frame objects (like
  C++ `decode_into`) or simd-json.
- **Signing**: a `create_order` takes about 13.4 µs, 11.7 µs of it in secp256k1. The SDK's part (order checks,
  hashing, r/s/v) is about 1.7 µs, mostly keccak, with 6 allocations (the r, s and signer strings the request
  keeps, and two short lists).

## Crate choices

| need | crate | why |
|---|---|---|
| async (client) | tokio 1 | the default Rust async runtime; what reqwest and tungstenite use |
| HTTP (client) | reqwest 0.13 (rustls) | keep-alive pool, timeouts, TLS without OpenSSL |
| TLS (client) | rustls on aws-lc-rs, OS trust store | REST and WebSocket trust the same roots (the OS store, so corporate CAs work); aws-lc-sys needs a C compiler to build, and cmake for some cross targets |
| WebSocket (client) | tokio-tungstenite 0.30 (rustls) | mature, handles ping/pong and TLS; fastwebsockets is faster at the frame level but needs its own HTTP upgrade and TLS glue, which is not where the time goes |
| secp256k1 | secp256k1 0.31 (libsecp256k1) | 11.8 µs vs 15.9 µs for k256 0.14 per recoverable sign. Not 0.33: it re-randomizes its global context on every signature, which doubles the cost (11.9 -> 23.5 µs); `PrivateKeySigner` blinds its own context once instead |
| keccak | sha3 0.12 | 360 ns vs 457 ns for tiny-keccak on 288 B |
| random | rand 0.10 | nonces straight from the OS source (`SysRng`); jitter from a seeded `StdRng` |
| decimals | rust_decimal | exact scaling and over-precision checks when signing |
| errors / logs | thiserror 2 / tracing | the usual choices |

## Design notes and TODOs

- **Builders, not positional arguments.** Requests have up to 11 required fields (transfer), so RPC methods
  return a fluent builder (the aws-sdk-rust style). Required fields are checked at send time, not at compile
  time.
- **`before_send` is a sync closure** that edits the request as JSON. An async hook would need a boxed future
  per call; not needed yet. Unset hooks cost one `Option` check.
- **Session rotation on WebSocket is break-before-make** (as in Python): after a re-login, a socket waits until
  no writes are in flight, then reconnects with the new cookie.
- **Frame-size check exists but is off**: `Environment::ws_max_frame_bytes` is `None` for the named
  environments. When set, an oversized frame is `Error::FrameTooLarge` and nothing is sent. Falling back to
  REST is a TODO.
- **Book deltas are never conflated.** The local `OrderBook` helper merges levels. Queue overflow breaks
  continuity.
- **Client order IDs use the wall clock** (`TimeCounterClientOrderId`), not the configured `Clock`.
- **Authentication belongs to the session runtime.** Raw edge login routes are not generated client methods.
- **What users can't do yet** (private-stream resume, wallet login, a blocking client) is listed in
  [Not supported yet](https://developers.grvt.io/sdks/rust#not-supported-yet); keep it there.
