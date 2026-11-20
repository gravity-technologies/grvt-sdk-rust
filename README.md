# grvt: Rust SDK for Grvt

The Rust SDK for the Grvt exchange: REST, WebSocket streams, WebSocket order entry and EIP-712 signing. One
crate, two layers:

- **Core** (always built): models, signing, request building, reply and stream-frame parsing, session state,
  sequencing, order books and recovery steps. No I/O, no async runtime, no tokio. For trading engines that run
  their own sockets, threads and event loop (mio, io_uring, busy-poll).
- **Client** (feature `client`, on by default): `Grvt`, an async tokio client built on the core.

> **Preview.** Any 0.x release can change the API. Pin an exact version.

## Install

```sh
cargo add grvt
```

In preview, any 0.x release can change the API, so pin an exact version: use an `=` requirement in
`Cargo.toml` (`grvt = "=<version>"`). Needs Rust 1.88 or newer. For the core alone, add `--no-default-features`.

## Quickstart

```rust
use grvt::{Config, Grvt, PrivateKeySigner};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The API key logs in; the signer signs orders with your signing key. Leave both out for public data only.
    let client = Grvt::new(
        Config::testnet()
            .api_key(std::env::var("GRVT_API_KEY")?)
            .signer(PrivateKeySigner::new(&std::env::var("GRVT_PRIVATE_KEY")?)?),
    )?;
    let t = client.ticker().instrument("BTC_USDT_Perp").await?;
    println!("{:?}", t.result.mark_price); // the exchange's fair price, used for margin
    client.close().await;
    Ok(())
}
```

Runnable examples: [`examples/quickstart.rs`](examples/quickstart.rs) (client) and
[`examples/order_and_subscribe.rs`](examples/order_and_subscribe.rs) (core only, fake transport). See also the [Quickstart guide](https://developers.grvt.io/guides/quickstart).

## Documentation

- [Rust SDK guide](https://developers.grvt.io/sdks/rust)
- [Quickstart](https://developers.grvt.io/guides/quickstart)
- [Signing](https://developers.grvt.io/guides/signing)
- [Errors and retries](https://developers.grvt.io/guides/errors-and-retries)
- [Recovery recipes](https://developers.grvt.io/guides/recovery)
- [WebSocket](https://developers.grvt.io/guides/websocket)
- [API reference](https://developers.grvt.io/reference/index)
- [API docs on docs.rs](https://docs.rs/grvt)

## Rust notes

| Feature | Default | Pulls in | Without it |
|---|---|---|---|
| `client` | on | tokio, reqwest, tokio-tungstenite, futures, bytes, tracing-subscriber | no `Grvt` client and no I/O; the core API stays |

- `client` is the only feature. `tracing` is always a dependency (logs go to target `grvt`); without `client`
  there is also no `GRVT_LOG` stderr logger.
- TLS is rustls on aws-lc-rs, with the OS trust store. No feature switches it to ring. aws-lc-sys needs a C
  compiler to build, and cmake for some cross targets.
- The client runs on tokio's multi-thread or current_thread runtime; the crate's client tests run on
  current_thread. Each subscription has a bounded queue (`.queue_size(n)` on the `Config`).
- Core parsing borrows: `parse_ws_message` returns a `WsMessage` that points into your buffer, and
  `ws_request_into` / `encode_request_into` write into a `Vec` you keep. Decoding a payload builds owned models.

### Performance

Measured with `benches/` (plain loops, median of 7 runs, counting allocator). The source does not record the
machine.

| Operation | Time | Heap allocations |
|---|---|---|
| Decode a 10+10 level book delta (core) | ~1.7 µs | 9 (the strings and lists the model owns) |
| Client stream path for that frame: parse, route, sequence, decode | ~2.9 µs | |
| Sign a `create_order`: 11.7 µs in secp256k1, 1.7 µs in the SDK (order checks, hashing, r/s/v) | ~13.4 µs | 6 in the SDK's part |

Rerun with `cargo bench --bench hot_path` (and `--bench lite` for full vs lite encoding) from a source
checkout. Details: [CONTRIBUTING.md](CONTRIBUTING.md#benchmarks).

## Known limitations

See [Not supported yet](https://developers.grvt.io/sdks/rust#not-supported-yet) on the Rust SDK page.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and [RUNTIME_API.md](RUNTIME_API.md).

## License

MIT. See [LICENSE](LICENSE).
