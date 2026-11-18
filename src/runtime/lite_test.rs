//! The lite encoding agrees with the contract (sdk/contract/contract.json), checked without trusting the
//! generator's own reading of it:
//!
//! - every generated lite codec: a fully populated full JSON sample of the model is renamed with the
//!   contract's lite names (per type, recursively) by the small converter below; the lite decoder must give the
//!   same model as the full decoder, and the lite encoder must give exactly the renamed JSON (so a request in
//!   lite carries only lite keys, and every field is renamed)
//! - the hand-written envelope pieces (JSON-RPC request and reply, stream frames, subscribe params and result,
//!   REST error body), REST paths and WebSocket paths use the contract's lite names.

use std::fmt::Debug;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use super::lite::{AsLite, Encoding, LiteDecode, LiteEncode};
use crate::generated::{lite::CHECKS, specs};
use crate::runtime::{Error, MessageKind, RpcOp};

use super::behavior_test::shared;
#[path = "../../tests/lite_names/mod.rs"]
mod lite_names;
use lite_names::LiteNames;

/// The full and lite JSON a model's lite encoder writes for a full sample.
pub(crate) type EncodeCheck = fn(&Value) -> Result<(Value, Value), String>;
/// Compares the lite decode of the lite sample with the full decode of the full sample.
pub(crate) type DecodeCheck = fn(&Value, &Value) -> Result<(), String>;
pub(crate) type Checks = (&'static str, Option<EncodeCheck>, Option<DecodeCheck>);

pub(crate) fn check_encode<T: DeserializeOwned + Serialize + LiteEncode>(
    full: &Value,
) -> Result<(Value, Value), String> {
    let model: T = serde_json::from_value(full.clone()).map_err(|e| format!("full decode: {e}"))?;
    let full = serde_json::to_value(&model).map_err(|e| e.to_string())?;
    let lite = serde_json::to_value(AsLite(&model)).map_err(|e| e.to_string())?;
    Ok((full, lite))
}

pub(crate) fn check_decode<T: DeserializeOwned + LiteDecode + PartialEq + Debug>(
    full: &Value,
    lite: &Value,
) -> Result<(), String> {
    let want: T = serde_json::from_value(full.clone()).map_err(|e| format!("full decode: {e}"))?;
    let got: T = Encoding::Lite
        .from_slice(&serde_json::to_vec(lite).unwrap())
        .map_err(|e| format!("lite decode: {e}"))?;
    if got != want {
        return Err(format!("lite decode differs:\n  lite: {got:?}\n  full: {want:?}"));
    }
    Ok(())
}

struct Contract {
    names: LiteNames,
    raw: Value,
}

impl Contract {
    fn load() -> Contract {
        let raw = shared::load("contract/contract.json");
        Contract {
            names: LiteNames::new(&raw),
            raw,
        }
    }
}

impl std::ops::Deref for Contract {
    type Target = LiteNames;
    fn deref(&self) -> &LiteNames {
        &self.names
    }
}

#[test]
fn every_lite_codec_matches_the_contract() {
    let c = Contract::load();
    let mut failures = Vec::new();
    let (mut encoders, mut decoders) = (0, 0);
    for (model, encode, decode) in CHECKS {
        let full = c.sample(model);
        let lite = c.to_lite(model, &full);
        assert!(
            full.as_object().unwrap().is_empty() || full != lite,
            "{model}: the sample has no renamed keys"
        );
        if let Some(check) = encode {
            encoders += 1;
            match check(&full) {
                Ok((full_out, lite_out)) => {
                    let want = c.to_lite(model, &full_out);
                    if lite_out != want {
                        failures.push(format!("{model}: lite encode\n  got  {lite_out}\n  want {want}"));
                    }
                }
                Err(e) => failures.push(format!("{model}: {e}")),
            }
        }
        if let Some(check) = decode {
            decoders += 1;
            if let Err(e) = check(&full, &lite) {
                failures.push(format!("{model}: {e}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(
        encoders > 10 && decoders > 100,
        "{encoders} encoders, {decoders} decoders"
    );
}

/// "s" is the stream in a frame, the size in a book level and the signer in a signature: one model with
/// repeated lite names nested in another.
#[test]
fn repeated_lite_names_rename_per_type() {
    let c = Contract::load();
    assert_eq!(c.lite("WSOrderbookLevelsFeedDataV1", "stream"), "s");
    assert_eq!(c.lite("OrderbookLevel", "size"), "s");
    let full = json!({"stream":"v1.book.s","selector":"BTC_USDT_Perp@500-10","sequence_number":"9",
        "prev_sequence_number":"8","feed":{"event_time":"1","instrument":"BTC_USDT_Perp",
        "bids":[{"price":"100.5","size":"2","num_orders":3}],"asks":[{"price":"101","size":"0.25","num_orders":1}]}});
    let lite = c.to_lite("WSOrderbookLevelsFeedDataV1", &full);
    assert_eq!(lite["s"], "v1.book.s");
    assert_eq!(lite["f"]["b"][0]["s"], "2");
    let want: crate::models::WSOrderbookLevelsFeedDataV1 = serde_json::from_value(full).unwrap();
    let got: crate::models::WSOrderbookLevelsFeedDataV1 =
        Encoding::Lite.from_slice(lite.to_string().as_bytes()).unwrap();
    assert_eq!(got, want);
}

/// Keys the contract does not list (Ticker frames carry `fi`) are skipped; full names in a lite body read as
/// nothing, which is why a missed rename must never reach the wire.
#[test]
fn lite_decode_skips_unknown_keys() {
    let level: crate::models::OrderbookLevel = Encoding::Lite
        .from_slice(br#"{"p":"1","s":"2","no":3,"fi":"x","zz":{"a":[1]}}"#)
        .unwrap();
    assert_eq!(level.size, rust_decimal::Decimal::TWO);
    assert!(Encoding::Lite
        .from_slice::<crate::models::OrderbookLevel>(br#"{"price":"1","size":"2","num_orders":3}"#)
        .is_err());
}

#[test]
fn envelope_uses_contract_lite_names() {
    let c = Contract::load();
    let ws = &c.raw["transport"]["ws"];
    let (req_t, resp_t, err_t) = (
        ws["request_type"].as_str().unwrap(),
        ws["response_type"].as_str().unwrap(),
        ws["error_type"].as_str().unwrap(),
    );
    let l = |t: &str, f: &str| c.lite(t, f);

    // JSON-RPC request: only JSONRPCRequest's lite keys
    let req = crate::models::ApiGetInstrumentRequest::new("BTC_USDT_Perp");
    let frame: Value =
        serde_json::from_str(&Encoding::Lite.ws_request::<crate::ops::GetInstrument>(&req, 7).unwrap()).unwrap();
    let want = json!({
        l(req_t, "jsonrpc"): "2.0", l(req_t, "method"): "v1/instrument", l(req_t, "id"): 7,
        l(req_t, "params"): c.to_lite("ApiGetInstrumentRequest", &json!({"instrument": "BTC_USDT_Perp"})),
    });
    assert_eq!(frame, want);

    // subscribe params
    let spec = &specs::STREAM_ORDERBOOK_ORDERBOOK_SNAP;
    let sub: Value = serde_json::from_str(&Encoding::Lite.subscribe_frame(spec, "X@500-10", 3)).unwrap();
    assert_eq!(
        sub[l(req_t, "params")],
        c.to_lite(
            "WSSubscribeParams",
            &json!({"stream": spec.stream, "selectors": ["X@500-10"], "use_global_sequence_number": true})
        )
    );

    // reply with a result, and a subscribe result in it
    let result = c.to_lite(
        "WSSubscribeResult",
        &json!({"stream": spec.stream, "subs": ["X@500-10"], "num_snapshots": [1], "latest_sequence_number": ["42"]}),
    );
    let reply =
        json!({l(resp_t, "jsonrpc"): "2.0", l(resp_t, "id"): 3, l(resp_t, "result"): result, l(resp_t, "method"): ""})
            .to_string();
    let msg = Encoding::Lite.parse_ws_message(reply.as_bytes()).unwrap();
    assert_eq!((msg.kind, msg.id), (MessageKind::Reply, Some(3)));
    let ack = Encoding::Lite
        .parse_subscribe_result(msg.result.unwrap(), "X@500")
        .unwrap();
    assert_eq!(
        (ack.selector.as_str(), ack.latest_sequence_number, ack.num_snapshots),
        ("X@500-10", 42, 1)
    );

    // error reply (id 0 and an empty method, as testnet sends)
    let error = json!({l(err_t, "code"): 1006, l(err_t, "message"): "slow down"});
    let reply = json!({l(resp_t, "id"): 9, l(resp_t, "error"): error, l(resp_t, "method"): ""}).to_string();
    let msg = Encoding::Lite.parse_ws_message(reply.as_bytes()).unwrap();
    assert!(matches!(crate::ws_result(&msg, "v1/x"), Err(Error::RateLimit { .. })));

    // REST-style error on WS: "s" is the status there, not a stream
    let http = &c.raw["transport"]["lite"]["http_error_body"];
    let rest_style = json!({l(resp_t, "id"): 5, http["code"].as_str().unwrap(): 2064,
        http["message"].as_str().unwrap(): "tick", http["status"].as_str().unwrap(): 400})
    .to_string();
    let msg = Encoding::Lite.parse_ws_message(rest_style.as_bytes()).unwrap();
    assert!(matches!(
        crate::ws_result(&msg, "v1/x"),
        Err(Error::Api { code: 2064, .. })
    ));

    // stream frame: routing fields and payload
    let feed_t = "WSOrderbookLevelsFeedDataV1";
    let frame = c.to_lite(
        feed_t,
        &json!({"stream": spec.stream, "selector": "X@500-10", "sequence_number": "0",
        "prev_sequence_number": "0", "feed": {"event_time": "1", "instrument": "X", "bids": [], "asks": []}}),
    );
    let text = frame.to_string();
    let msg = Encoding::Lite.parse_ws_message(text.as_bytes()).unwrap();
    assert_eq!(msg.kind, MessageKind::Feed);
    assert_eq!(
        (msg.stream.as_deref(), msg.selector.as_deref()),
        (Some(spec.stream), Some("X@500-10"))
    );
    let decoded = crate::decode_feed::<crate::ops::OrderbookSnapStream>(&msg).unwrap();
    assert_eq!(decoded.feed.instrument, "X");

    // REST error body
    let body = c.raw["transport"]["lite"]["error_example"].as_str().unwrap();
    let err = Encoding::Lite
        .rest_response_body(crate::ops::GetInstrument::SPEC, 400, body.as_bytes())
        .unwrap_err();
    assert!(matches!(err, Error::Api { code: 1003, .. }), "{err:?}");
}

#[test]
fn paths_use_the_contract_templates() {
    let c = Contract::load();
    let template = c.raw["transport"]["lite"]["http_path_template"].as_str().unwrap();
    for rpc in c.raw["rpcs"].as_array().unwrap() {
        let spec = specs::RPCS
            .iter()
            .find(|s| s.path == rpc["http_path"].as_str().unwrap())
            .unwrap();
        let want = template
            .replace("{version}", &rpc["version"].to_string())
            .replace("{route}", rpc["route"].as_str().unwrap());
        assert_eq!(spec.path_for(Encoding::Lite), want);
        assert_eq!(spec.path_for(Encoding::Full), spec.path);
    }
    for spec in specs::RPCS
        .iter()
        .filter(|s| s.gateway == crate::runtime::Gateway::Edge)
    {
        assert_eq!(spec.path_for(Encoding::Lite), spec.path, "edge stays full");
        assert_eq!(spec.encoding(Encoding::Lite), Encoding::Full);
    }
    for env in crate::Environment::all() {
        let want = &c.raw["environments"][&env.name];
        for (g, key) in [
            (crate::runtime::Gateway::MarketData, "market_data_ws_lite"),
            (crate::runtime::Gateway::Trading, "trading_ws_lite"),
        ] {
            assert_eq!(env.ws_url_for(g, Encoding::Lite).unwrap(), want[key].as_str().unwrap());
        }
    }
}
