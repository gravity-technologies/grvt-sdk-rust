//! The generated EIP-712 encoders (`signing::hash_*`) against the reference: build the typed message
//! (`signing::message_*`) and hash it with the generic encoder (tests/reference). Many random
//! requests per signed type, including empty and absent lists, integer extremes, negative int64, max
//! uint256 and multi-leg orders. Rejected requests must fail with the same message.

use grvt::runtime::{CurrencyMeta, InstrumentMeta, Meta};
use grvt::Timestamp;
use grvt::{enums as e, models as m, signing, StructType, Value};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use rust_decimal::Decimal;

mod reference;

const ROUNDS: usize = 3000;

fn compare<R>(
    name: &str,
    st: &StructType,
    req: &R,
    meta: &Meta,
    message: fn(&R, &Meta) -> grvt::Result<Value>,
    hash: fn(&R, &Meta) -> grvt::Result<[u8; 32]>,
    tally: &mut (usize, usize),
) {
    let want = message(req, meta).and_then(|v| reference::hash_struct(st, &v));
    let got = hash(req, meta);
    match (&want, &got) {
        (Ok(a), Ok(b)) => {
            assert_eq!(a, b, "{name}: digest");
            tally.0 += 1;
        }
        (Err(a), Err(b)) => {
            assert_eq!(a.to_string(), b.to_string(), "{name}: error");
            tally.1 += 1;
        }
        _ => panic!("{name}: reference {want:?} vs generated {got:?}"),
    }
}

/// Mostly `Some(v)`, sometimes absent.
fn maybe<T>(r: &mut StdRng, v: impl FnOnce(&mut StdRng) -> T) -> Option<T> {
    if r.random_ratio(19, 20) {
        Some(v(r))
    } else {
        None
    }
}

fn u64_any(r: &mut StdRng) -> u64 {
    match r.random_range(0..6) {
        0 => 0,
        1 => u64::MAX,
        _ => r.random(),
    }
}

fn i64_any(r: &mut StdRng) -> i64 {
    match r.random_range(0..8) {
        0 => i64::MIN,
        1 => i64::MAX,
        2 => -r.random_range(1..1_000_000_000i64),
        _ => r.random(),
    }
}

fn u32_any(r: &mut StdRng) -> u32 {
    if r.random_ratio(1, 6) {
        u32::MAX
    } else {
        r.random()
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn address(r: &mut StdRng) -> String {
    match r.random_range(0..20) {
        0 => String::new(),
        1 => "0x0000000000000000000000000000000000000000".into(),
        2 => "0xnot-an-address".into(),
        3 => format!("0X{}", hex(&r.random::<[u8; 20]>()).to_uppercase()),
        _ => format!("0x{}", hex(&r.random::<[u8; 20]>())),
    }
}

/// A decimal string with up to `decimals` places (rarely too precise, too large or malformed).
fn decimal(r: &mut StdRng, decimals: u32) -> Decimal {
    let text: String = match r.random_range(0..40) {
        0 => "1.0000000000000000001".into(),
        1 => "99999999999999999999999".into(),
        2 => "0.00000000000000000001".into(),
        3 => "0".into(),
        4 => "-1".into(),
        _ => {
            let int: u64 = r.random_range(0..10_000_000);
            let places = r.random_range(0..=decimals.min(9));
            if places == 0 {
                int.to_string()
            } else {
                let frac: u64 = r.random_range(0..10u64.pow(places));
                format!("{int}.{frac:0width$}", width = places as usize)
            }
        }
    };
    Decimal::from_str_exact(&text).unwrap()
}

fn meta(r: &mut StdRng) -> Meta {
    let mut meta = Meta::default();
    for i in 0..3 {
        let hash = match i {
            0 => format!("0x{}", "f".repeat(64)), // max uint256
            1 => "0x030501".into(),
            _ => format!("0x{}", hex(&r.random::<[u8; 32]>())),
        };
        meta.instruments.insert(
            format!("I{i}"),
            InstrumentMeta {
                instrument_hash: hash,
                base_decimals: r.random_range(0..=9),
                ..Default::default()
            },
        );
    }
    for i in 0..3 {
        let id = match i {
            0 => 255,
            1 => r.random_range(256..70_000), // too wide for uint8, sometimes for uint16
            _ => r.random_range(0..256),
        };
        let symbol = format!("C{i}");
        meta.currencies.insert(
            symbol.clone(),
            CurrencyMeta {
                id,
                symbol,
                balance_decimals: r.random_range(0..=9),
            },
        );
    }
    meta.main_account = match r.random_range(0..20) {
        0 => None,
        1 => Some("0xbad".into()),
        _ => Some(format!("0x{}", hex(&r.random::<[u8; 20]>()))),
    };
    meta
}

fn instrument(r: &mut StdRng) -> String {
    format!("I{}", r.random_range(0..4)) // I3 has no metadata
}

fn currency(r: &mut StdRng) -> String {
    format!("C{}", r.random_range(0..3))
}

/// A signed decimal with up to `decimals` places, or one of the bad values.
fn signed_decimal(r: &mut StdRng, decimals: u32) -> Decimal {
    match r.random_range(0..30) {
        0 => Decimal::from_str_exact("-9223372036854775809").unwrap(), // below int64 once scaled
        1 => Decimal::from_str_exact("9223372036854775807").unwrap(),
        n => {
            let d = decimal(r, decimals);
            if n % 2 == 0 && !d.is_sign_negative() {
                -d
            } else {
                d
            }
        }
    }
}

fn add_margin(r: &mut StdRng) -> m::ApiAddIsolatedPositionMarginRequestInput {
    {
        let mut value: m::ApiAddIsolatedPositionMarginRequestInput = Default::default();
        value.sub_account_id = (maybe(r, u64_any)).unwrap_or_default();
        value.instrument = (maybe(r, instrument)).unwrap_or_default();
        value.amount = (maybe(r, |r| signed_decimal(r, 9))).unwrap_or_default();
        value.signature = maybe(r, signature);
        value
    }
}

fn derisk(r: &mut StdRng) -> m::ApiSetDeriskToMaintenanceMarginRatioRequestInput {
    let mut request = {
        let mut value: m::ApiSetDeriskToMaintenanceMarginRatioRequestInput = Default::default();
        value.sub_account_id = (maybe(r, u64_any)).unwrap_or_default();
        value
    }
    // uint32 after scaling by 10^6: mostly small ratios, sometimes out of range.
    .with_ratio(
        (maybe(r, |r| {
            if r.random_ratio(1, 5) {
                decimal(r, 6)
            } else {
                Decimal::from_str_exact(&format!("{}.{:02}", r.random_range(0..4000), r.random_range(0..100))).unwrap()
            }
        }))
        .unwrap_or_default(),
    );
    request.signature = maybe(r, signature);
    request
}

fn margin_config(r: &mut StdRng) -> m::ApiSetSubAccountPositionMarginConfigRequestInput {
    let types = [e::PositionMarginType::Isolated, e::PositionMarginType::Cross];
    let mut request = {
        let mut value: m::ApiSetSubAccountPositionMarginConfigRequestInput = Default::default();
        value.sub_account_id = (maybe(r, u64_any)).unwrap_or_default();
        value.instrument = (maybe(r, instrument)).unwrap_or_default();
        value.margin_type = match r.random_range(0..15) {
            0 => Default::default(),
            1 => e::PositionMarginType::Unknown("NEW".into()),
            n => types[n % 2].clone(),
        };
        value
    }
    // int32 after scaling: mostly small leverages, sometimes any signed decimal.
    .with_leverage(
        (maybe(r, |r| {
            if r.random_ratio(1, 5) {
                signed_decimal(r, 2)
            } else {
                Decimal::from_str_exact(&format!("{}.{}", r.random_range(-100..100), r.random_range(0..10))).unwrap()
            }
        }))
        .unwrap_or_default(),
    );
    request.signature = maybe(r, signature);
    request
}

fn vault_burn(r: &mut StdRng) -> m::ApiVaultBurnTokensRequestInput {
    {
        let mut value: m::ApiVaultBurnTokensRequestInput = Default::default();
        value.vault_id = (maybe(r, u64_any)).unwrap_or_default();
        value.currency = (maybe(r, currency)).unwrap_or_default();
        value.num_tokens = (maybe(r, |r| decimal(r, 6))).unwrap_or_default();
        value.signature = maybe(r, signature);
        value
    }
}

fn vault_invest(r: &mut StdRng) -> m::ApiVaultInvestV2RequestInput {
    {
        let mut value: m::ApiVaultInvestV2RequestInput = Default::default();
        value.vault_id = (maybe(r, u64_any)).unwrap_or_default();
        value.from_sub_account_id = (maybe(r, u64_any)).unwrap_or_default();
        value.from_wallet_type = (wallet(r)).unwrap_or_default();
        value.from_currency = (maybe(r, currency)).unwrap_or_default();
        value.from_num_tokens = (maybe(r, |r| decimal(r, 9))).unwrap_or_default();
        value.to_currency = (maybe(r, currency)).unwrap_or_default();
        value.exchange_rate = (maybe(r, |r| decimal(r, 9))).unwrap_or_default();
        value.signature = maybe(r, signature);
        value
    }
}

fn vault_redeem(r: &mut StdRng) -> m::ApiVaultRedeemV2RequestInput {
    {
        let mut value: m::ApiVaultRedeemV2RequestInput = Default::default();
        value.vault_id = (maybe(r, u64_any)).unwrap_or_default();
        value.from_currency = (maybe(r, currency)).unwrap_or_default();
        value.to_currency = (maybe(r, currency)).unwrap_or_default();
        value.num_tokens = (maybe(r, |r| decimal(r, 9))).unwrap_or_default();
        value.exchange_rate = (maybe(r, |r| decimal(r, 9))).unwrap_or_default();
        value.signature = maybe(r, signature);
        value
    }
}

fn signature(r: &mut StdRng) -> m::SignatureInput {
    {
        let mut value: m::SignatureInput = Default::default();
        value.nonce = maybe(r, u32_any);
        value.expiration = maybe(r, |r| Timestamp::from_nanoseconds(i64_any(r)));
        value
    }
}

fn order(r: &mut StdRng) -> m::ApiCreateOrderRequestInput {
    let tifs = e::TimeInForce::ALL;
    let legs = match r.random_range(0..10) {
        0 => None,
        1 => Some(Vec::new()),
        _ => Some(
            (0..r.random_range(1..5))
                .map(|_| {
                    // I3 has no metadata.
                    {
                        let mut value: m::OrderLeg = Default::default();
                        value.instrument = (maybe(r, |r| format!("I{}", r.random_range(0..4)))).unwrap_or_default();
                        value.size = (maybe(r, |r| decimal(r, 9))).unwrap_or_default();
                        value.limit_price = maybe(r, |r| decimal(r, 9));
                        value.is_buying_asset = r.random();
                        value
                    }
                })
                .collect(),
        ),
    };
    let tif = match r.random_range(0..30) {
        0 => Some(e::TimeInForce::Unknown("NEW_TIF".into())),
        1 => None,
        _ => Some(tifs[r.random_range(0..tifs.len())].clone()),
    };
    {
        let mut value: m::ApiCreateOrderRequestInput = Default::default();
        value.order = {
            let mut value: m::OrderInput = Default::default();
            value.sub_account_id = maybe(r, u64_any);
            value.is_market = r.random_ratio(1, 2).then(|| r.random());
            value.time_in_force = (tif).unwrap_or_default();
            value.post_only = r.random_ratio(1, 2).then(|| r.random());
            value.reduce_only = r.random_ratio(1, 2).then(|| r.random());
            value.legs = legs;
            value.signature = maybe(r, signature);
            value.builder = r.random_ratio(1, 2).then(|| address(r));
            value.builder_fee = r.random_ratio(3, 4).then(|| decimal(r, 6));
            value
        };
        value
    }
}

fn wallet(r: &mut StdRng) -> Option<e::WalletType> {
    match r.random_range(0..12) {
        0 => None,
        1 => Some(e::WalletType::Unknown("NEW".into())),
        n => Some([e::WalletType::Funding, e::WalletType::Spot, e::WalletType::Futures][n % 3].clone()),
    }
}

fn transfer(r: &mut StdRng) -> m::ApiTransferRequestInput {
    {
        let mut value: m::ApiTransferRequestInput = Default::default();
        value.from_account_id = (maybe(r, address)).unwrap_or_default();
        value.from_sub_account_id = (maybe(r, u64_any)).unwrap_or_default();
        value.to_account_id = (maybe(r, address)).unwrap_or_default();
        value.to_sub_account_id = (maybe(r, u64_any)).unwrap_or_default();
        value.currency = (maybe(r, |r| format!("C{}", r.random_range(0..3)))).unwrap_or_default();
        value.num_tokens = (maybe(r, |r| decimal(r, 9))).unwrap_or_default();
        value.signature = maybe(r, signature);
        value.from_wallet_type = wallet(r);
        value.to_wallet_type = wallet(r);
        value
    }
}

fn withdrawal(r: &mut StdRng) -> m::ApiWithdrawalRequestInput {
    {
        let mut value: m::ApiWithdrawalRequestInput = Default::default();
        value.from_account_id = (maybe(r, address)).unwrap_or_default();
        value.to_eth_address = (maybe(r, address)).unwrap_or_default();
        value.currency = (maybe(r, |r| format!("C{}", r.random_range(0..3)))).unwrap_or_default();
        value.num_tokens = (maybe(r, |r| decimal(r, 9))).unwrap_or_default();
        value.signature = maybe(r, signature);
        value
    }
}

fn login(r: &mut StdRng) -> m::EdgeWalletLoginRequestInput {
    {
        let mut value: m::EdgeWalletLoginRequestInput = Default::default();
        value.address = (maybe(r, address)).unwrap_or_default();
        value.signature = maybe(r, signature);
        value
    }
}

#[test]
fn generated_encoders_match_reference() {
    let mut r = StdRng::seed_from_u64(0x5eed);
    let mut tally = std::collections::BTreeMap::<&str, (usize, usize)>::new();
    for _ in 0..ROUNDS {
        let meta = meta(&mut r);
        let o = order(&mut r);
        let t = transfer(&mut r);
        let w = withdrawal(&mut r);
        let l = login(&mut r);
        let am = add_margin(&mut r);
        let dr = derisk(&mut r);
        let mc = margin_config(&mut r);
        let vb = vault_burn(&mut r);
        let vi = vault_invest(&mut r);
        let vr = vault_redeem(&mut r);
        macro_rules! check {
            ($name:literal, $st:ident, $req:expr, $msg:path, $hash:path) => {
                compare(
                    $name,
                    &signing::$st,
                    $req,
                    &meta,
                    $msg,
                    $hash,
                    tally.entry($name).or_default(),
                )
            };
        }
        check!(
            "OrderInput",
            EIP712_ORDER,
            &o,
            signing::message_order,
            signing::hash_order
        );
        check!(
            "OrderWithBuilderFee",
            EIP712_ORDER_WITH_BUILDER_FEE,
            &o,
            signing::message_order_with_builder_fee,
            signing::hash_order_with_builder_fee
        );
        check!(
            "Transfer",
            EIP712_TRANSFER,
            &t,
            signing::message_transfer,
            signing::hash_transfer
        );
        check!(
            "TransferV2",
            EIP712_TRANSFER_V2,
            &t,
            signing::message_transfer_v2,
            signing::hash_transfer_v2
        );
        check!(
            "TransferV3",
            EIP712_TRANSFER_V3,
            &t,
            signing::message_transfer_v3,
            signing::hash_transfer_v3
        );
        check!(
            "Withdrawal",
            EIP712_WITHDRAWAL,
            &w,
            signing::message_withdrawal,
            signing::hash_withdrawal
        );
        check!(
            "WithdrawalV2",
            EIP712_WITHDRAWAL_V2,
            &w,
            signing::message_withdrawal_v2,
            signing::hash_withdrawal_v2
        );
        check!(
            "WalletLogin",
            EIP712_WALLET_LOGIN,
            &l,
            signing::message_wallet_login,
            signing::hash_wallet_login
        );
        check!(
            "AddIsolatedPositionMargin",
            EIP712_ADD_ISOLATED_POSITION_MARGIN,
            &am,
            signing::message_add_isolated_position_margin,
            signing::hash_add_isolated_position_margin
        );
        check!(
            "SetDeriskToMaintenanceMarginRatio",
            EIP712_SET_DERISK_TO_MAINTENANCE_MARGIN_RATIO,
            &dr,
            signing::message_set_derisk_to_maintenance_margin_ratio,
            signing::hash_set_derisk_to_maintenance_margin_ratio
        );
        check!(
            "SetSubAccountPositionMarginConfig",
            EIP712_SET_SUB_ACCOUNT_POSITION_MARGIN_CONFIG,
            &mc,
            signing::message_set_sub_account_position_margin_config,
            signing::hash_set_sub_account_position_margin_config
        );
        check!(
            "VaultBurnLpToken",
            EIP712_VAULT_BURN_LP_TOKEN,
            &vb,
            signing::message_vault_burn_lp_token,
            signing::hash_vault_burn_lp_token
        );
        check!(
            "VaultInvestV3",
            EIP712_VAULT_INVEST_V3,
            &vi,
            signing::message_vault_invest_v3,
            signing::hash_vault_invest_v3
        );
        check!(
            "VaultRedeemV3",
            EIP712_VAULT_REDEEM_V3,
            &vr,
            signing::message_vault_redeem_v3,
            signing::hash_vault_redeem_v3
        );
    }
    // AddSessionKey is in the contract but has no mapping, so nothing signs it.
    assert_eq!(tally.len(), 14);
    // Both outcomes must be well exercised for every type.
    for (name, (ok, err)) in &tally {
        assert!(
            *ok >= ROUNDS / 20 && *err >= 20,
            "{name}: {ok} matched digests, {err} matched errors"
        );
    }
}

/// Out-of-range values fail with the reference message (struct.field prefix, type, value).
#[test]
fn out_of_range_errors() {
    let mut meta = Meta::default();
    meta.instruments.insert(
        "I".into(),
        InstrumentMeta {
            instrument_hash: "0x01".into(),
            base_decimals: 9,
            ..Default::default()
        },
    );
    meta.currencies.insert(
        "WIDE".into(),
        CurrencyMeta {
            id: 300,
            symbol: "WIDE".into(),
            balance_decimals: 6,
        },
    );
    let mut o = {
        let mut value: m::ApiCreateOrderRequestInput = Default::default();
        value.order = {
            let mut value: m::OrderInput = Default::default();
            value.sub_account_id = Some(1);
            value.time_in_force = e::TimeInForce::GoodTillTime;
            value.legs = Some(vec![{
                let mut value: m::OrderLeg = Default::default();
                value.instrument = "I".into();
                value
            }
            // 1e20 > u64::MAX after scaling.
            .with_size(Decimal::from_str_exact("100000000000").unwrap())]);
            value.signature = Some({
                let mut value: m::SignatureInput = Default::default();
                value.nonce = Some(1);
                value.expiration = Some(Timestamp::from_nanoseconds(-5));
                value
            });
            value
        };
        value
    };
    let err = signing::hash_order(&o, &meta).unwrap_err().to_string();
    let want = signing::message_order(&o, &meta)
        .and_then(|v| reference::hash_struct(&signing::EIP712_ORDER, &v))
        .unwrap_err()
        .to_string();
    assert_eq!(err, want);
    assert!(
        err.contains("OrderLeg.contractSize: value 100000000000000000000 out of range for uint64"),
        "{err}"
    );

    // A later absent field wins over an earlier range error, as in the reference.
    o.order.signature.as_mut().unwrap().nonce = None;
    let err = signing::hash_order(&o, &meta).unwrap_err().to_string();
    assert!(err.contains("order.signature.nonce is required"), "{err}");

    let t = {
        let mut value: m::ApiTransferRequestInput = Default::default();
        value.currency = "WIDE".into();
        value.num_tokens = Decimal::from_str_exact("1").unwrap();
        value.from_sub_account_id = 0;
        value.to_sub_account_id = 0;
        value.signature = Some({
            let mut value: m::SignatureInput = Default::default();
            value.nonce = Some(0);
            value.expiration = Some(Timestamp::from_nanoseconds(0));
            value
        });
        value
    };
    let err = signing::hash_transfer(&t, &meta).unwrap_err().to_string();
    assert!(
        err.contains("Transfer.tokenCurrency: value 300 out of range for uint8"),
        "{err}"
    );
    assert!(signing::hash_transfer_v3(&t, &meta).is_ok());
}
