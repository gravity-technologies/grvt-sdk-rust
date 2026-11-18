//! Replay every server-produced signing case in sdk/scenarios/signing.json through the CLIENT's signing (the
//! generated sign pieces driven by the async `Signer`): byte-equal domain separator, struct hash, digest,
//! typed data, r/s/v, signature object and signed request; reject cases must fail before anything is
//! signed. tests/core_signing.rs replays the same cases without a client.
#![cfg(feature = "client")]

use std::sync::{Arc, Mutex};

mod reference;
mod shared;

use futures::future::BoxFuture;
use grvt::runtime::clock::FixedClock;
use grvt::runtime::signing::hex0x;
use grvt::runtime::{CurrencyMeta, InstrumentMeta};
use grvt::{
    models as m, Config, Domain, Environment, Error, Grvt, PrivateKeySigner, Signature, Signer, StructType, Value,
};
use serde_json::Value as Json;

#[derive(Debug, Clone)]
struct Seen {
    domain_separator: String,
    struct_name: &'static str,
    struct_hash: String,
    digest: String,
    typed_data: Json,
}

/// Captures what the generated code asked to sign, then signs for real.
struct RecordingSigner {
    inner: PrivateKeySigner,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Signer for RecordingSigner {
    fn address(&self) -> &str {
        self.inner.address()
    }

    fn sign_typed<'a>(
        &'a self,
        domain: &'a Domain,
        st: &'a StructType,
        message: &'a Value,
    ) -> BoxFuture<'a, grvt::Result<Signature>> {
        self.seen.lock().unwrap().push(Seen {
            domain_separator: hex0x(&domain.separator()),
            struct_name: st.name,
            struct_hash: hex0x(&reference::hash_struct(st, message).unwrap()),
            digest: hex0x(&reference::digest(st, domain, message).unwrap()),
            typed_data: st.to_typed_data(domain, message).unwrap(),
        });
        let digest = reference::digest(st, domain, message).unwrap();
        Box::pin(std::future::ready(Ok(self.inner.sign_digest(&digest))))
    }

    fn sign_prehash(&self, _: &[u8; 32]) -> Option<grvt::Result<Signature>> {
        None // record the typed message instead
    }
}

fn signing_cases() -> Json {
    shared::load("scenarios/signing.json")
}

async fn sign_case(client: &Grvt, rpc: &str, wire: &Json) -> grvt::Result<Json> {
    // The wire request decodes, re-encodes to exactly the wire JSON, then the client signs it.
    macro_rules! signed {
        ($req:ty, $sign:ident) => {{
            let req: $req = serde_json::from_value(wire.clone()).expect("wire request decodes");
            assert_eq!(&serde_json::to_value(&req).unwrap(), wire);
            client.$sign(req).await.map(|s| serde_json::to_value(&s).unwrap())
        }};
    }
    match rpc {
        "order.create_order" => {
            signed!(m::ApiCreateOrderRequestInput, sign_order_create_order)
        }
        "order.bulk_orders" => {
            signed!(m::ApiBulkOrdersRequestInput, sign_order_bulk_orders)
        }
        "transfer.transfer" => {
            signed!(m::ApiTransferRequestInput, sign_transfer_transfer)
        }
        "transfer.withdrawal" => {
            signed!(m::ApiWithdrawalRequestInput, sign_transfer_withdrawal)
        }
        "edge.wallet_login" => {
            signed!(m::EdgeWalletLoginRequestInput, sign_edge_wallet_login)
        }
        "derisk_mm_ratio.set_derisk_mm_ratio" => {
            signed!(
                m::ApiSetDeriskToMaintenanceMarginRatioRequestInput,
                sign_derisk_mm_ratio_set_derisk_mm_ratio
            )
        }
        "position.add_position_margin" => {
            signed!(
                m::ApiAddIsolatedPositionMarginRequestInput,
                sign_position_add_position_margin
            )
        }
        "position.set_position_config" => {
            signed!(
                m::ApiSetSubAccountPositionMarginConfigRequestInput,
                sign_position_set_position_config
            )
        }
        "vault.vault_burn_tokens" => {
            signed!(m::ApiVaultBurnTokensRequestInput, sign_vault_vault_burn_tokens)
        }
        "vault.vault_invest" => {
            signed!(m::ApiVaultInvestV2RequestInput, sign_vault_vault_invest)
        }
        "vault.vault_redeem" => {
            signed!(m::ApiVaultRedeemV2RequestInput, sign_vault_vault_redeem)
        }
        other => panic!("no binding for {other}"),
    }
}

fn client_for(vec: &Json, key: &str, seen: Arc<Mutex<Vec<Seen>>>) -> Grvt {
    let signer = RecordingSigner {
        inner: PrivateKeySigner::new(key).unwrap(),
        seen,
    };
    client_with(vec, signer)
}

fn client_with(vec: &Json, signer: impl Signer + 'static) -> Grvt {
    let client = Grvt::new(Config::new(Environment::testnet()).signer(signer).clock(FixedClock(0))).unwrap();
    let meta = &vec["inputs"]["metadata"];
    for inst in meta["instruments"].as_array().into_iter().flatten() {
        let v: InstrumentMeta = serde_json::from_value(inst.clone()).unwrap();
        client
            .runtime()
            .metadata()
            .put_instrument(inst["instrument"].as_str().unwrap(), v);
    }
    for cur in meta["currencies"].as_array().into_iter().flatten() {
        let v: CurrencyMeta = serde_json::from_value(cur.clone()).unwrap();
        client
            .runtime()
            .metadata()
            .put_currency(cur["symbol"].as_str().unwrap(), v);
    }
    if let Some(a) = meta["main_account_id"].as_str() {
        client.runtime().metadata().put_main_account(a);
    }
    client
}

#[tokio::test]
async fn every_signing_case() {
    let all = signing_cases();
    let key = all["test_key"]["private_key"].as_str().unwrap();
    let address = all["test_key"]["address"].as_str().unwrap();
    assert_eq!(PrivateKeySigner::new(key).unwrap().address(), address);
    let mut rpcs = std::collections::BTreeSet::new();
    let list = all["cases"].as_array().unwrap();
    for vec in list {
        let name = vec["name"].as_str().unwrap();
        let rpc = vec["rpc"].as_str().unwrap();
        rpcs.insert(rpc.to_owned());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let client = client_for(vec, key, seen.clone());
        assert_eq!(
            client.runtime().env().chain_id,
            vec["inputs"]["env_chain_id"].as_u64().unwrap()
        );
        let wire = &vec["inputs"]["wire_request"];
        let out = sign_case(&client, rpc, wire).await;
        // The same case through the fast path: PrivateKeySigner signs the digest of the generated
        // encoder directly (the recording signer above gets the typed message instead).
        let fast = sign_case(&client_with(vec, PrivateKeySigner::new(key).unwrap()), rpc, wire).await;
        match (&out, &fast) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "{name}: fast path signs the same bytes"),
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{name}: fast path error"),
            (a, b) => panic!("{name}: typed path {a:?} vs fast path {b:?}"),
        }
        if vec["expect"] == "reject" {
            match out {
                Err(Error::Signing(msg)) => assert!(msg.contains("decimal places"), "{name}: {msg}"),
                other => panic!("{name}: expected a signing error, got {other:?}"),
            }
            assert!(seen.lock().unwrap().is_empty(), "{name}: refused before signing");
            continue;
        }
        let signed = out.unwrap_or_else(|e| panic!("{name}: {e}"));
        let seen = seen.lock().unwrap().clone();
        if let Some(elements) = vec["elements"].as_array() {
            // Per-element binding: one signature per list element, in list order.
            assert_eq!(seen.len(), elements.len(), "{name}: one signature per element");
            for (s, el) in seen.iter().zip(elements) {
                let path = el["path"].as_str().unwrap();
                let (list_name, index) = path.trim_end_matches(']').split_once('[').unwrap();
                let sig = &signed[list_name][index.parse::<usize>().unwrap()]["signature"];
                check_steps(
                    &format!("{name} {path}"),
                    s,
                    el["signed_type"].as_str().unwrap(),
                    &el["steps"],
                    sig,
                );
            }
            assert_eq!(&signed, &vec["signed_request"], "{name}: signed request");
            continue;
        }
        let steps = &vec["steps"];
        let sig = if rpc == "order.create_order" {
            &signed["order"]["signature"]
        } else {
            &signed["signature"]
        };
        check_steps(name, &seen[0], vec["signed_type"].as_str().unwrap(), steps, sig);
        assert_eq!(&signed, &steps["signed_request"], "{name}: signed request");
    }
    assert_eq!(list.len(), 20);
    let expected: std::collections::BTreeSet<String> = [
        "derisk_mm_ratio.set_derisk_mm_ratio",
        "position.add_position_margin",
        "position.set_position_config",
        "vault.vault_burn_tokens",
        "vault.vault_invest",
        "vault.vault_redeem",
        "order.bulk_orders",
        "order.create_order",
        "transfer.transfer",
        "transfer.withdrawal",
        "edge.wallet_login",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(rpcs, expected, "every signed call has a signing case");
}

/// One signature against its signing case steps: typed data, hashes, digest, r/s/v and the signature object.
fn check_steps(name: &str, s: &Seen, signed_type: &str, steps: &Json, sig: &Json) {
    assert_eq!(s.struct_name, signed_type, "{name}");
    assert_eq!(
        s.domain_separator,
        steps["domain_separator"].as_str().unwrap(),
        "{name}: domain separator"
    );
    assert_eq!(
        s.struct_hash,
        steps["struct_hash"].as_str().unwrap(),
        "{name}: struct hash"
    );
    assert_eq!(s.digest, steps["digest"].as_str().unwrap(), "{name}: digest");
    assert_eq!(s.typed_data, steps["typed_data"], "{name}: typed data");
    assert_eq!(
        (&sig["r"], &sig["s"], &sig["v"]),
        (&steps["r"], &steps["s"], &steps["v"]),
        "{name}: r/s/v"
    );
    assert_eq!(sig, &steps["signature"], "{name}: signature object");
}

/// A typed-data signer whose signature does not recover to its address (here: it signs other bytes)
/// is refused after it returns.
#[tokio::test]
async fn wrong_typed_signature_is_refused() {
    struct Liar(PrivateKeySigner);
    impl Signer for Liar {
        fn address(&self) -> &str {
            self.0.address()
        }
        fn sign_typed<'a>(
            &'a self,
            _: &'a Domain,
            _: &'a StructType,
            _: &'a Value,
        ) -> BoxFuture<'a, grvt::Result<Signature>> {
            Box::pin(std::future::ready(Ok(self.0.sign_digest(&[7u8; 32]))))
        }
        fn sign_prehash(&self, _: &[u8; 32]) -> Option<grvt::Result<Signature>> {
            None
        }
    }
    let all = signing_cases();
    let key = all["test_key"]["private_key"].as_str().unwrap();
    let vec = all["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["rpc"] == "edge.wallet_login" && v["expect"] != "reject")
        .unwrap();
    let client = client_with(vec, Liar(PrivateKeySigner::new(key).unwrap()));
    let err = sign_case(&client, "edge.wallet_login", &vec["inputs"]["wire_request"])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Signing(m) if m.contains("invalid signature: it recovers to")),
        "{err}"
    );
}

/// Every SDK must reach the same verdict, nearest values and message on the shared order-check cases.
#[tokio::test]
async fn order_checks() {
    let all = signing_cases();
    let key = all["test_key"]["private_key"].as_str().unwrap();
    let cases = all["order_checks"].as_array().unwrap();
    assert!(!cases.is_empty());
    for vec in cases {
        let name = vec["name"].as_str().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let signer = RecordingSigner {
            inner: PrivateKeySigner::new(key).unwrap(),
            seen: seen.clone(),
        };
        let client = Grvt::new(Config::new(Environment::testnet()).signer(signer).clock(FixedClock(0))).unwrap();
        for inst in vec["instruments"].as_array().unwrap() {
            let mut v: InstrumentMeta = serde_json::from_value(inst.clone()).unwrap();
            for k in ["tick_size", "min_size", "min_notional"] {
                v.limits.insert(k.to_owned(), inst[k].as_str().unwrap().to_owned());
            }
            client
                .runtime()
                .metadata()
                .put_instrument(inst["instrument"].as_str().unwrap(), v);
        }
        let order = serde_json::from_value(vec["order"].clone());
        if vec["expect"] == "invalid" && order.is_err() {
            assert!(seen.lock().unwrap().is_empty());
            continue;
        }
        let req = m::ApiCreateOrderRequestInput::default().with_order(order.unwrap());
        let out = client.sign_order_create_order(req).await;
        let signed = seen.lock().unwrap().len();
        let shown = out.as_ref().err().map(ToString::to_string);
        match (vec["expect"].as_str().unwrap(), out) {
            ("pass", Ok(_)) => assert_eq!(signed, 1, "{name}"),
            ("invalid", Err(Error::Signing(_))) => assert_eq!(signed, 0, "{name}"),
            ("reject", Err(Error::OrderOutOfBounds(e))) => {
                let r = &vec["rejection"];
                let got = serde_json::json!({
                    "instrument": e.instrument, "leg": e.leg, "field": e.field, "check": e.check,
                    "limit": e.limit, "value": e.value, "limit_value": e.limit_value,
                    "nearest": e.nearest, "server_code": e.server_code, "message": e.message,
                });
                assert_eq!(&got, r, "{name}");
                assert_eq!(shown.as_deref(), r["message"].as_str(), "{name}");
                assert_eq!(signed, 0, "{name}");
            }
            (want, got) => panic!("{name}: want {want}, got {got:?}"),
        }
    }
}

/// `check_orders(false)` leaves the limits to the exchange: every order the check would refuse is signed.
#[tokio::test]
async fn order_checks_off_signs_rejected_orders() {
    let all = signing_cases();
    let key = all["test_key"]["private_key"].as_str().unwrap();
    let cases: Vec<_> = all["order_checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["expect"] == "reject")
        .collect();
    assert!(!cases.is_empty());
    for vec in cases {
        let name = vec["name"].as_str().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let signer = RecordingSigner {
            inner: PrivateKeySigner::new(key).unwrap(),
            seen: seen.clone(),
        };
        let config = Config::new(Environment::testnet())
            .signer(signer)
            .clock(FixedClock(0))
            .check_orders(false);
        let client = Grvt::new(config).unwrap();
        for inst in vec["instruments"].as_array().unwrap() {
            let mut v: InstrumentMeta = serde_json::from_value(inst.clone()).unwrap();
            for k in ["tick_size", "min_size", "min_notional"] {
                v.limits.insert(k.to_owned(), inst[k].as_str().unwrap().to_owned());
            }
            client
                .runtime()
                .metadata()
                .put_instrument(inst["instrument"].as_str().unwrap(), v);
        }
        let req =
            m::ApiCreateOrderRequestInput::default().with_order(serde_json::from_value(vec["order"].clone()).unwrap());
        let out = client.sign_order_create_order(req).await;
        assert!(out.is_ok(), "{name}: {out:?}");
        assert_eq!(seen.lock().unwrap().len(), 1, "{name}");
    }
}
