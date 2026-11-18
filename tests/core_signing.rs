//! Core signing without a client: every server signing case in sdk/scenarios/signing.json through the generated
//! `sign_<call>(&mut req, &ctx)` with a `SyncSigner`, the metadata `needs_<call>` names, the two-step
//! `digests_` / `attach_` split for bulk orders, and the signature checks.

use std::collections::BTreeSet;
use std::sync::Mutex;

use grvt::runtime::signing::hex0x;
use grvt::{
    models as m, signing, CurrencyMeta, Error, InstrumentMeta, Meta, PrivateKeySigner, Result, SignContext, Signature,
    SigningRequest, SyncSigner, Timestamp,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value as Json;

mod shared;

fn signing_cases() -> Json {
    shared::load("scenarios/signing.json")
}

fn test_key(all: &Json) -> PrivateKeySigner {
    PrivateKeySigner::new(all["test_key"]["private_key"].as_str().unwrap()).unwrap()
}

#[derive(Debug, Clone)]
struct Seen {
    primary_type: &'static str,
    domain_separator: String,
    struct_hash: String,
    digest: String,
    typed_data: Json,
}

/// Records what it is asked to sign (including the typed data a wallet would see), then signs.
struct Recording {
    key: PrivateKeySigner,
    seen: Mutex<Vec<Seen>>,
}

impl SyncSigner for Recording {
    fn address(&self) -> &str {
        self.key.address()
    }

    fn sign(&self, r: &SigningRequest<'_>) -> Result<Signature> {
        self.seen.lock().unwrap().push(Seen {
            primary_type: r.primary_type(),
            domain_separator: hex0x(&r.domain_separator),
            struct_hash: hex0x(&r.struct_hash),
            digest: hex0x(&r.digest),
            typed_data: r.typed_data()?,
        });
        self.key.sign(r)
    }
}

fn meta_of(vec: &Json) -> Meta {
    let src = &vec["inputs"]["metadata"];
    let mut meta = Meta::default();
    for inst in src["instruments"].as_array().into_iter().flatten() {
        let v: InstrumentMeta = serde_json::from_value(inst.clone()).unwrap();
        meta.instruments
            .insert(inst["instrument"].as_str().unwrap().to_owned(), v);
    }
    for cur in src["currencies"].as_array().into_iter().flatten() {
        let v: CurrencyMeta = serde_json::from_value(cur.clone()).unwrap();
        meta.currencies.insert(cur["symbol"].as_str().unwrap().to_owned(), v);
    }
    meta.main_account = src["main_account_id"].as_str().map(str::to_owned);
    meta
}

/// One call's generated core functions, typed.
struct Call<Req> {
    sign: fn(&mut Req, &SignContext<'_>) -> Result<()>,
    needs: fn(&Req) -> grvt::SigningNeeds,
}

fn run<Req: Serialize + DeserializeOwned>(call: Call<Req>, vec: &Json, ctx: &SignContext<'_>) -> Result<Json> {
    let wire = &vec["inputs"]["wire_request"];
    let mut req: Req = serde_json::from_value(wire.clone()).expect("wire request decodes");
    assert_eq!(&serde_json::to_value(&req).unwrap(), wire);
    // needs_ names everything sign_ reads: signing with only that metadata gives the same bytes.
    let needs = (call.needs)(&req);
    let mut only = Meta::default();
    for name in &needs.instruments {
        if let Some(v) = ctx.metadata.instruments.get(name) {
            only.instruments.insert(name.clone(), v.clone());
        }
    }
    for symbol in &needs.currencies {
        if let Some(v) = ctx.metadata.currencies.get(symbol) {
            only.currencies.insert(symbol.clone(), v.clone());
        }
    }
    if needs.main_account {
        only.main_account = ctx.metadata.main_account.clone();
    }
    let narrow = SignContext {
        metadata: &only,
        ..*ctx
    };
    let mut narrow_req: Req = serde_json::from_value(wire.clone()).unwrap();
    let narrow_out = (call.sign)(&mut narrow_req, &narrow).map(|()| serde_json::to_value(&narrow_req).unwrap());

    (call.sign)(&mut req, ctx)?;
    let out = serde_json::to_value(&req).unwrap();
    assert_eq!(
        narrow_out.as_ref().ok(),
        Some(&out),
        "{}: needs_ is complete",
        vec["name"]
    );
    Ok(out)
}

fn sign_case(rpc: &str, vec: &Json, ctx: &SignContext<'_>) -> Result<Json> {
    macro_rules! call {
        ($req:ty, $sign:ident, $needs:ident) => {
            run::<$req>(
                Call {
                    sign: signing::$sign,
                    needs: signing::$needs,
                },
                vec,
                ctx,
            )
        };
    }
    match rpc {
        "order.create_order" => call!(
            m::ApiCreateOrderRequestInput,
            sign_order_create_order,
            needs_order_create_order
        ),
        "order.bulk_orders" => call!(
            m::ApiBulkOrdersRequestInput,
            sign_order_bulk_orders,
            needs_order_bulk_orders
        ),
        "transfer.transfer" => call!(
            m::ApiTransferRequestInput,
            sign_transfer_transfer,
            needs_transfer_transfer
        ),
        "transfer.withdrawal" => call!(
            m::ApiWithdrawalRequestInput,
            sign_transfer_withdrawal,
            needs_transfer_withdrawal
        ),
        "edge.wallet_login" => call!(
            m::EdgeWalletLoginRequestInput,
            sign_edge_wallet_login,
            needs_edge_wallet_login
        ),
        "derisk_mm_ratio.set_derisk_mm_ratio" => call!(
            m::ApiSetDeriskToMaintenanceMarginRatioRequestInput,
            sign_derisk_mm_ratio_set_derisk_mm_ratio,
            needs_derisk_mm_ratio_set_derisk_mm_ratio
        ),
        "position.add_position_margin" => call!(
            m::ApiAddIsolatedPositionMarginRequestInput,
            sign_position_add_position_margin,
            needs_position_add_position_margin
        ),
        "position.set_position_config" => call!(
            m::ApiSetSubAccountPositionMarginConfigRequestInput,
            sign_position_set_position_config,
            needs_position_set_position_config
        ),
        "vault.vault_burn_tokens" => call!(
            m::ApiVaultBurnTokensRequestInput,
            sign_vault_vault_burn_tokens,
            needs_vault_vault_burn_tokens
        ),
        "vault.vault_invest" => call!(
            m::ApiVaultInvestV2RequestInput,
            sign_vault_vault_invest,
            needs_vault_vault_invest
        ),
        "vault.vault_redeem" => call!(
            m::ApiVaultRedeemV2RequestInput,
            sign_vault_vault_redeem,
            needs_vault_vault_redeem
        ),
        other => panic!("no binding for {other}"),
    }
}

fn check_steps(name: &str, s: &Seen, signed_type: &str, steps: &Json, sig: &Json) {
    assert_eq!(s.primary_type, signed_type, "{name}");
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

#[test]
fn every_signing_case_without_a_client() {
    let all = signing_cases();
    let key = test_key(&all);
    assert_eq!(key.address(), all["test_key"]["address"].as_str().unwrap());
    let list = all["cases"].as_array().unwrap();
    let mut rpcs = BTreeSet::new();
    for vec in list {
        let name = vec["name"].as_str().unwrap();
        let rpc = vec["rpc"].as_str().unwrap();
        rpcs.insert(rpc.to_owned());
        let meta = meta_of(vec);
        let chain_id = vec["inputs"]["env_chain_id"].as_u64().unwrap();
        let recording = Recording {
            key: key.clone(),
            seen: Mutex::new(Vec::new()),
        };
        let mut ctx = SignContext::new(&recording, &meta, chain_id);
        ctx.now = Some(Timestamp::from_nanoseconds(0));
        let out = sign_case(rpc, vec, &ctx);
        // The same with every signature recovered, and with a plain key (no typed data asked for).
        let mut checked = SignContext::new(&key, &meta, chain_id);
        checked.now = ctx.now;
        checked.recover_signer = true;
        let plain = sign_case(rpc, vec, &checked);
        match (&out, &plain) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "{name}: plain key signs the same bytes"),
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{name}"),
            (a, b) => panic!("{name}: recording {a:?} vs plain {b:?}"),
        }
        let seen = recording.seen.lock().unwrap().clone();
        if vec["expect"] == "reject" {
            match out {
                Err(Error::Signing(msg)) => assert!(msg.contains("decimal places"), "{name}: {msg}"),
                other => panic!("{name}: expected a signing error, got {other:?}"),
            }
            assert!(seen.is_empty(), "{name}: refused before signing");
            continue;
        }
        let signed = out.unwrap_or_else(|e| panic!("{name}: {e}"));
        if let Some(elements) = vec["elements"].as_array() {
            // Two signatures per element: the run with only needs_ metadata, then the real one.
            assert_eq!(seen.len(), 2 * elements.len(), "{name}: one signature per element");
            for (s, el) in seen[elements.len()..].iter().zip(elements) {
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
        check_steps(name, &seen[1], vec["signed_type"].as_str().unwrap(), steps, sig);
        assert_eq!(&signed, &steps["signed_request"], "{name}: signed request");
    }
    assert_eq!(rpcs.len(), 11, "every signed call has a signing case");
}

fn bulk_case(all: &Json) -> (&Json, m::ApiBulkOrdersRequestInput) {
    let vec = all["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["rpc"] == "order.bulk_orders")
        .unwrap();
    let req = serde_json::from_value(vec["inputs"]["wire_request"].clone()).unwrap();
    (vec, req)
}

#[test]
fn bulk_orders_sign_elsewhere_then_attach() {
    let all = signing_cases();
    let key = test_key(&all);
    let (vec, mut req) = bulk_case(&all);
    let meta = meta_of(vec);
    let mut ctx = SignContext::new(&key, &meta, 326);
    ctx.now = Some(Timestamp::from_nanoseconds(0));

    assert!(
        matches!(signing::attach_order_bulk_orders(&mut req.clone(), &ctx, &[]), Err(Error::Config(m)) if m.contains("signatures for 2 elements")),
        "wrong count"
    );
    let digests = signing::digests_order_bulk_orders(&mut req, &ctx).unwrap();
    assert_eq!(digests.len(), 2);
    assert!(
        matches!(digests[0].typed_data(), Err(Error::Config(_))),
        "digests carry no typed data"
    );
    assert_eq!(
        signing::needs_order_bulk_orders(&req).instruments,
        vec!["BTC_USDT_Perp".to_owned()]
    );

    // Sign on other threads.
    let signatures: Vec<Signature> = std::thread::scope(|s| {
        let handles: Vec<_> = digests
            .iter()
            .map(|d| {
                let key = &key;
                s.spawn(move || key.sign_digest(&d.digest))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Swapped signatures are refused even with recover_signer off, and nothing is written.
    let swapped = [signatures[1].clone(), signatures[0].clone()];
    let before = serde_json::to_value(&req).unwrap();
    let err = signing::attach_order_bulk_orders(&mut req, &ctx, &swapped).unwrap_err();
    assert!(matches!(&err, Error::Signing(m) if m.contains("recovers to")), "{err}");
    assert_eq!(serde_json::to_value(&req).unwrap(), before);

    signing::attach_order_bulk_orders(&mut req, &ctx, &signatures).unwrap();
    assert_eq!(serde_json::to_value(&req).unwrap(), vec["signed_request"]);
}

#[test]
fn attach_needs_prepared_elements() {
    let all = signing_cases();
    let key = test_key(&all);
    let (vec, mut req) = bulk_case(&all);
    let meta = meta_of(vec);
    let ctx = SignContext::new(&key, &meta, 326);
    for order in req.orders.iter_mut().flatten() {
        order.signature = None;
    }
    let sigs = [key.sign_digest(&[1; 32]), key.sign_digest(&[2; 32])];
    let err = signing::attach_order_bulk_orders(&mut req, &ctx, &sigs).unwrap_err();
    assert!(matches!(&err, Error::Config(m) if m.contains("digests")), "{err}");
}

#[test]
fn signer_output_is_checked() {
    struct Tampered(PrivateKeySigner, fn(&mut Signature));
    impl SyncSigner for Tampered {
        fn address(&self) -> &str {
            self.0.address()
        }
        fn sign(&self, r: &SigningRequest<'_>) -> Result<Signature> {
            let mut s = self.0.sign_digest(&r.digest);
            (self.1)(&mut s);
            Ok(s)
        }
    }
    let all = signing_cases();
    let (vec, req) = bulk_case(&all);
    let meta = meta_of(vec);
    let key = test_key(&all);
    let other = PrivateKeySigner::new("0x01").err();
    assert!(other.is_some(), "short keys are refused");
    let wrong_key = PrivateKeySigner::new(&format!("0x{}", "11".repeat(32))).unwrap();

    type Tamper = fn(&mut Signature);
    let cases: [(Tamper, &str); 2] = [
        (|s| s.v = 29, "v must be 27 or 28"),
        (|s| s.s = format!("0x{}", "ff".repeat(32)), "not low-s"),
    ];
    for (tamper, want) in cases {
        let signer = Tampered(key.clone(), tamper);
        let ctx = SignContext::new(&signer, &meta, 326);
        let err = signing::sign_order_bulk_orders(&mut req.clone(), &ctx).unwrap_err();
        assert!(err.to_string().contains(want), "{err}");
    }

    // A signer using another key: only recovering the address catches it.
    struct Impostor<'a>(&'a PrivateKeySigner, &'a PrivateKeySigner);
    impl SyncSigner for Impostor<'_> {
        fn address(&self) -> &str {
            self.0.address()
        }
        fn sign(&self, r: &SigningRequest<'_>) -> Result<Signature> {
            let mut s = self.1.sign_digest(&r.digest);
            s.signer = String::new();
            Ok(s)
        }
    }
    let impostor = Impostor(&key, &wrong_key);
    let mut ctx = SignContext::new(&impostor, &meta, 326);
    assert!(signing::sign_order_bulk_orders(&mut req.clone(), &ctx).is_ok());
    ctx.recover_signer = true;
    let err = signing::sign_order_bulk_orders(&mut req.clone(), &ctx).unwrap_err();
    assert!(err.to_string().contains("recovers to"), "{err}");
}

#[test]
fn defaults_fill_the_signature_object() {
    let all = signing_cases();
    let key = test_key(&all);
    let (vec, _) = bulk_case(&all);
    let meta = meta_of(vec);
    let leg =
        m::OrderLeg::new("BTC_USDT_Perp", "0.01".parse().unwrap(), true).with_limit_price("60000".parse().unwrap());
    let order = m::OrderInput::new(grvt::enums::TimeInForce::GoodTillTime, Some(vec![leg])).with_sub_account_id(1);
    let mut req = m::ApiCreateOrderRequestInput::new(order);
    struct Seven;
    impl grvt::runtime::NonceProvider for Seven {
        fn next_nonce(&self) -> u32 {
            7
        }
    }
    let mut ctx = SignContext::new(&key, &meta, 326);
    ctx.now = Some(Timestamp::from_nanoseconds(1_000));
    ctx.nonces = Some(&Seven);
    signing::sign_order_create_order(&mut req, &ctx).unwrap();
    let sig = req.order.signature.as_ref().unwrap();
    assert_eq!(sig.signer.as_deref(), Some(key.address()));
    assert_eq!(sig.nonce, Some(7));
    assert_eq!(sig.chain_id, Some(326));
    assert_eq!(
        sig.expiration.map(Timestamp::nanoseconds),
        Some(1_000 + signing::SIGNATURE_TTL_NS)
    );
    assert!(sig.r.is_some() && sig.s.is_some() && sig.v.is_some());

    // Without a nonce source the OS random source is used.
    let mut again = m::ApiCreateOrderRequestInput::new(req.order.clone().with_signature(m::SignatureInput::default()));
    signing::sign_order_create_order(&mut again, &SignContext::new(&key, &meta, 326)).unwrap();
    assert!(again.order.signature.unwrap().nonce.is_some());
}

#[test]
fn chain_id_zero_is_refused() {
    let all = signing_cases();
    let key = test_key(&all);
    let (vec, mut req) = bulk_case(&all);
    let meta = meta_of(vec);
    for order in req.orders.iter_mut().flatten() {
        order.signature = None;
    }
    let err = signing::sign_order_bulk_orders(&mut req, &SignContext::new(&key, &meta, 0)).unwrap_err();
    assert!(matches!(&err, Error::Config(m) if m.contains("chain_id is 0")), "{err}");
    assert!(
        req.orders.iter().flatten().all(|o| o.signature.is_none()),
        "nothing written"
    );
}

#[test]
fn attach_refuses_a_request_changed_after_digests() {
    let all = signing_cases();
    let key = test_key(&all);
    let (vec, mut req) = bulk_case(&all);
    let meta = meta_of(vec);
    let ctx = SignContext::new(&key, &meta, 326);
    let digests = signing::digests_order_bulk_orders(&mut req, &ctx).unwrap();
    let signatures: Vec<_> = digests.iter().map(|d| key.sign_digest(&d.digest)).collect();
    let leg = &mut req.orders.as_mut().unwrap()[0].legs.as_mut().unwrap()[0];
    leg.limit_price = Some("1".parse().unwrap());
    let err = signing::attach_order_bulk_orders(&mut req, &ctx, &signatures).unwrap_err();
    assert!(matches!(&err, Error::Signing(m) if m.contains("recovers to")), "{err}");
}

#[test]
fn a_failing_element_changes_no_element() {
    let all = signing_cases();
    let key = test_key(&all);
    let (vec, mut req) = bulk_case(&all);
    let meta = meta_of(vec);
    let orders = req.orders.as_mut().unwrap();
    orders[0].signature = None;
    orders[1].legs.as_mut().unwrap()[0].size = "0.0000000001".parse().unwrap(); // over-precise
    let before = serde_json::to_value(&req).unwrap();
    assert!(signing::digests_order_bulk_orders(&mut req, &SignContext::new(&key, &meta, 326)).is_err());
    assert_eq!(serde_json::to_value(&req).unwrap(), before);
}

#[test]
fn a_failing_nonce_source_stops_signing() {
    struct Broken;
    impl grvt::runtime::NonceProvider for Broken {
        fn next_nonce(&self) -> u32 {
            unreachable!()
        }
        fn try_next_nonce(&self) -> Result<u32> {
            Err(Error::Signing("no randomness".into()))
        }
    }
    let all = signing_cases();
    let key = test_key(&all);
    let (vec, mut req) = bulk_case(&all);
    let meta = meta_of(vec);
    for order in req.orders.iter_mut().flatten() {
        order.signature = None;
    }
    let mut ctx = SignContext::new(&key, &meta, 326);
    ctx.nonces = Some(&Broken);
    let err = signing::sign_order_bulk_orders(&mut req, &ctx).unwrap_err();
    assert!(matches!(&err, Error::Signing(m) if m == "no randomness"), "{err}");
}

fn check_meta(vec: &Json) -> Meta {
    let mut meta = Meta::default();
    for inst in vec["instruments"].as_array().unwrap() {
        let mut v: InstrumentMeta = serde_json::from_value(inst.clone()).unwrap();
        for k in ["tick_size", "min_size", "min_notional"] {
            v.limits.insert(k.to_owned(), inst[k].as_str().unwrap().to_owned());
        }
        meta.instruments
            .insert(inst["instrument"].as_str().unwrap().to_owned(), v);
    }
    meta
}

/// The order check through `check_order` (JSON), through signing (typed), and signing with checks off.
fn three_ways(order: m::OrderInput, meta: &Meta, signer: &PrivateKeySigner) -> (Result<()>, Result<()>, Result<()>) {
    let json = grvt::runtime::signing::check_order(&signing::ORDER_CHECKS, Some(&order), meta);
    let mut ctx = SignContext::new(signer, meta, 326);
    ctx.now = Some(Timestamp::from_nanoseconds(1_700_000_000_000_000_000));
    let mut req = m::ApiCreateOrderRequestInput::default().with_order(order);
    let mut unchecked = req.clone();
    let typed = signing::sign_order_create_order(&mut req, &ctx);
    ctx.check_orders = false;
    let off = signing::sign_order_create_order(&mut unchecked, &ctx);
    (json, typed, off)
}

/// The sign functions read the order field by field; `check_order` reads its JSON. Both must reach the
/// same verdict and the same error, and match the expected outcome of every shared order-check case.
#[test]
fn order_checks_typed_and_json_agree() {
    let all = signing_cases();
    let signer = test_key(&all);
    let mut skipped = BTreeSet::new();
    for vec in all["order_checks"].as_array().unwrap() {
        let name = vec["name"].as_str().unwrap();
        let expect = vec["expect"].as_str().unwrap();
        let Ok(order) = serde_json::from_value::<m::OrderInput>(vec["order"].clone()) else {
            skipped.insert(name);
            assert_eq!(expect, "invalid", "{name} does not decode");
            continue;
        };
        let (json, typed, _) = three_ways(order, &check_meta(vec), &signer);
        match (expect, json, typed) {
            ("pass", Ok(()), Ok(())) => {}
            ("invalid", Err(Error::Signing(a)), Err(Error::Signing(b))) => assert_eq!(a, b, "{name}"),
            ("reject", Err(Error::OrderOutOfBounds(a)), Err(Error::OrderOutOfBounds(b))) => {
                assert_eq!(a, b, "{name}");
                let r = &vec["rejection"];
                assert_eq!(a.message, r["message"].as_str().unwrap(), "{name}");
                assert_eq!(a.field, r["field"].as_str().unwrap(), "{name}");
                assert_eq!(a.server_code, r["server_code"].as_u64().unwrap() as u32, "{name}");
            }
            (want, a, b) => panic!("{name}: want {want}, check_order {a:?}, sign {b:?}"),
        }
    }
    // Only a decimal the model itself refuses never reaches the check.
    assert_eq!(skipped, BTreeSet::from(["malformed_size"]));

    // Mutated orders: both paths agree on every one, and a passing check signs exactly as with checks off.
    let cases = all["order_checks"].as_array().unwrap();
    let decimals = [
        "0",
        "0.0005",
        "0.001",
        "0.0015",
        "0.01",
        "1",
        "3000.005",
        "60000",
        "60000.03",
        "60000.1",
        "18446744073.7",
        "18446744073.709551615",
        "0.0010000001",
        "-1",
        "100000000000000000000",
    ];
    let names = ["BTC_USDT_Perp", "ETH_USDT_Perp", "SOL_USDT_Perp", ""];
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = |n: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % n as u64) as usize
    };
    let (mut compared, mut passed) = (0, 0);
    for _ in 0..3000 {
        let vec = &cases[next(cases.len())];
        let mut order = vec["order"].clone();
        for _ in 0..1 + next(3) {
            let leg_count = order["legs"].as_array().map_or(0, Vec::len);
            let i = next(leg_count.max(1));
            let (d, n) = (decimals[next(decimals.len())], names[next(names.len())]);
            match next(9) {
                0 if leg_count > 0 => order["legs"][i]["size"] = d.into(),
                1 if leg_count > 0 => order["legs"][i]["limit_price"] = d.into(),
                2 if leg_count > 0 => {
                    order["legs"][i].as_object_mut().unwrap().remove("limit_price");
                }
                3 if leg_count > 0 => order["legs"][i]["instrument"] = n.into(),
                4 => order["is_market"] = (next(2) == 1).into(),
                5 => order["reduce_only"] = (next(2) == 1).into(),
                6 => {
                    order["metadata"]["trigger"] = serde_json::json!({
                        "trigger_type": "TAKE_PROFIT",
                        "tpsl": {"trigger_by": "LAST", "trigger_price": "60000", "close_position": next(2) == 1,
                            "is_split_position": false},
                    })
                }
                7 => order["legs"] = Json::Array(Vec::new()),
                _ => order["legs"] = Json::Null,
            }
        }
        let Ok(model) = serde_json::from_value::<m::OrderInput>(order.clone()) else {
            continue;
        };
        let (json, typed, off) = three_ways(model, &check_meta(vec), &signer);
        compared += 1;
        let shown = |r: &Result<()>| format!("{r:?}");
        match &json {
            Ok(()) => {
                passed += 1;
                assert_eq!(shown(&typed), shown(&off), "{order}");
            }
            Err(_) => assert_eq!(shown(&json), shown(&typed), "{order}"),
        }
    }
    assert!(
        compared > 1000 && passed > 100 && compared - passed > 100,
        "{compared} compared, {passed} passed"
    );
}
