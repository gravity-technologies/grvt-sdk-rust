//! Order checks: instrument limits (tick, minimum size, size step, minimum notional) checked before
//! signing.
//!
//! The rules come from the contract (`order_checks`) through the generated `ORDER_CHECKS` table; this
//! module only applies them. Values are compared as exact integers, scaled like the signed fields.
//! Nothing is ever rounded or changed: a failing order is `Error::OrderOutOfBounds`, with the nearest
//! valid values.

use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value as Json;

use super::wire::{scale_decimal, scale_decimal_value, Meta};
use crate::runtime::error::{Error, OrderOutOfBounds, Result};

/// How a leg field becomes an integer: fixed decimals, or the instrument's `base_decimals`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    Fixed(u32),
    BaseDecimals,
}

/// One contract rule (contract.json `order_checks.rules`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderCheckRule {
    pub field: &'static str,
    /// With a second field, the rule checks `field * times`.
    pub times: Option<&'static str>,
    /// "at_least" or "multiple_of".
    pub check: &'static str,
    pub limit: &'static str,
    pub skip_if: &'static [&'static str],
    pub server_code: u32,
}

/// The generated `ORDER_CHECKS` table.
#[derive(Debug, Clone, Copy)]
pub struct OrderChecks {
    pub legs: &'static str,
    pub instrument: &'static str,
    pub scales: &'static [(&'static str, Scale)],
    pub rules: &'static [OrderCheckRule],
}

/// n / 10^d without trailing zeros: (60000100000000, 9) -> "60000.1".
fn fmt(n: u128, d: u32) -> String {
    let s = n.to_string();
    if d == 0 {
        return s;
    }
    let d = d as usize;
    let s = if s.len() <= d {
        format!("{}{s}", "0".repeat(d + 1 - s.len()))
    } else {
        s
    };
    let (whole, frac) = s.split_at(s.len() - d);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole}.{frac}")
    }
}

fn ceil_div(a: u128, b: u128) -> u128 {
    a / b + u128::from(!a.is_multiple_of(b))
}

fn dig<'a>(v: &'a Json, path: &str) -> Option<&'a Json> {
    path.split('.').try_fold(v, |cur, k| cur.get(k))
}

/// A leg field as the checks read it: the wire string, or the model's `Decimal` (read as the string it
/// encodes to).
#[derive(Debug, Clone, Copy)]
pub(crate) enum LegValue<'a> {
    Text(&'a str),
    Decimal(&'a Decimal),
}

impl<'a> From<&'a String> for LegValue<'a> {
    fn from(s: &'a String) -> Self {
        LegValue::Text(s)
    }
}

impl<'a> From<&'a Decimal> for LegValue<'a> {
    fn from(d: &'a Decimal) -> Self {
        LegValue::Decimal(d)
    }
}

impl std::fmt::Display for LegValue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LegValue::Text(s) => f.write_str(s),
            LegValue::Decimal(d) => d.fmt(f),
        }
    }
}

impl LegValue<'_> {
    fn scale(self, decimals: u32) -> std::result::Result<i128, String> {
        match self {
            LegValue::Text(s) => scale_decimal(s, decimals),
            LegValue::Decimal(d) => scale_decimal_value(*d, decimals),
        }
    }
}

/// One order leg as the generated sign functions hand it to `check_order_legs`: the instrument and the
/// value of each `OrderChecks::scales` field, in that order (`None` when absent).
#[derive(Debug, Clone, Copy)]
pub(crate) struct CheckLeg<'a, const N: usize> {
    pub instrument: &'a str,
    pub values: [Option<LegValue<'a>>; N],
}

/// `Error::OrderOutOfBounds` for the first leg value outside its instrument's limits, or
/// `Error::Signing` for a malformed value. `order` is the wire Order.
pub fn check_order<T: Serialize>(checks: &OrderChecks, order: Option<&T>, meta: &Meta) -> Result<()> {
    let Some(order) = order else { return Ok(()) };
    let order = serde_json::to_value(order).map_err(|e| Error::Signing(format!("order: {e}")))?;
    let Some(legs) = order.get(checks.legs).and_then(Json::as_array) else {
        return Ok(());
    };
    let skipped = |r: &OrderCheckRule| r.skip_if.iter().any(|p| dig(&order, p) == Some(&Json::Bool(true)));
    for (index, leg) in legs.iter().enumerate() {
        let name = leg.get(checks.instrument).and_then(Json::as_str).unwrap_or("");
        let raw = |field: &str| leg.get(field).and_then(Json::as_str).map(LegValue::Text);
        check_leg(checks, name, raw, &skipped, index, meta)?;
    }
    Ok(())
}

/// `check_order` for an order the generated sign functions read field by field, without encoding it:
/// `skip` holds the order's `skip_if` paths that are true.
pub(crate) fn check_order_legs<'a, const N: usize>(
    checks: &OrderChecks,
    skip: &[Option<&str>],
    legs: impl IntoIterator<Item = CheckLeg<'a, N>>,
    meta: &Meta,
) -> Result<()> {
    const {
        assert!(
            N <= SCALES_KEPT,
            "more scales fields than check_leg keeps: raise SCALES_KEPT"
        )
    };
    if checks.scales.len() != N {
        return Err(Error::Config(format!(
            "order checks: {N} leg values for {} scales fields",
            checks.scales.len()
        )));
    }
    let skipped = |r: &OrderCheckRule| r.skip_if.iter().any(|p| skip.contains(&Some(*p)));
    for (index, leg) in legs.into_iter().enumerate() {
        let raw = |field: &str| {
            let i = checks.scales.iter().position(|(f, _)| *f == field)?;
            leg.values.get(i).copied().flatten()
        };
        check_leg(checks, leg.instrument, raw, &skipped, index, meta)?;
    }
    Ok(())
}

/// Scaled leg values kept per leg (the contract has two scales fields; `check_order_legs` fails to
/// compile for more).
const SCALES_KEPT: usize = 4;

fn check_leg<'v>(
    checks: &OrderChecks,
    name: &str,
    raw: impl Fn(&str) -> Option<LegValue<'v>>,
    skipped: &dyn Fn(&OrderCheckRule) -> bool,
    index: usize,
    meta: &Meta,
) -> Result<()> {
    let inst = meta.instrument_named(name)?;
    let decimals = |field: &str| match checks.scales.iter().find(|(f, _)| *f == field).map(|(_, s)| *s) {
        Some(Scale::Fixed(d)) => d,
        _ => inst.base_decimals,
    };
    let value = |field: &str| -> Result<Option<u128>> {
        let raw = match raw(field) {
            None | Some(LegValue::Text("")) => return Ok(None),
            Some(v) => v,
        };
        let n = raw
            .scale(decimals(field))
            .map_err(|e| Error::Signing(format!("OrderLeg.{field}: {e}")))?;
        if n < 0 || n > u64::MAX as i128 {
            return Err(Error::Signing(format!("OrderLeg.{field}: {raw} is out of range")));
        }
        Ok(Some(n as u128))
    };
    let limit = |field: &str, d: u32| -> Result<u128> {
        match inst.limits.get(field).map(String::as_str) {
            None | Some("") => Ok(0),
            Some(raw) => scale_decimal(raw, d)
                .map(|n| n.max(0) as u128)
                .map_err(|e| Error::Signing(format!("instrument {name} {field}: {e}"))),
        }
    };

    // Every scales field once: a malformed value is reported as such, whatever the rules.
    let mut scaled = [None; SCALES_KEPT];
    for (i, (field, _)) in checks.scales.iter().enumerate() {
        let v = value(field)?;
        if let Some(slot) = scaled.get_mut(i) {
            *slot = Some(v);
        }
    }
    let value = |field: &str| -> Result<Option<u128>> {
        match checks
            .scales
            .iter()
            .position(|(f, _)| *f == field)
            .and_then(|i| scaled.get(i))
        {
            Some(Some(v)) => Ok(*v),
            _ => value(field),
        }
    };
    for r in checks.rules {
        if skipped(r) {
            continue;
        }
        let Some(mut v) = value(r.field)? else { continue };
        let mut d = decimals(r.field);
        if let Some(times) = r.times {
            let Some(t) = value(times)? else { continue };
            if v == 0 || t == 0 {
                continue;
            }
            v *= t;
            d += decimals(times);
        }
        let lim = limit(r.limit, d)?;
        if lim == 0 {
            continue;
        }
        let passes = match r.check {
            "at_least" => v >= lim,
            "multiple_of" => v % lim == 0,
            _ => true,
        };
        if passes {
            continue;
        }
        // Built only for a failing rule.
        let label = match r.times {
            Some(times) => format!("{} * {times}", r.field),
            None => r.field.to_owned(),
        };
        let (nearest, nd, msg) = match (r.check, r.times) {
            ("at_least", None) => (
                vec![lim],
                d,
                format!("{label} {} is below {} {}", fmt(v, d), r.limit, fmt(lim, d)),
            ),
            ("at_least", Some(times)) => {
                let nd = decimals(times);
                let mut x = ceil_div(lim, value(r.field)?.unwrap_or(1));
                for r2 in checks.rules {
                    if r2.field != times || r2.times.is_some() || skipped(r2) {
                        continue;
                    }
                    let l2 = limit(r2.limit, nd)?;
                    if l2 == 0 {
                        continue;
                    }
                    if r2.check == "at_least" {
                        x = x.max(l2);
                    } else {
                        x = ceil_div(x, l2) * l2;
                    }
                }
                let msg = format!(
                    "{label} = {} is below {} {} (smallest {times} at this {}: {})",
                    fmt(v, d),
                    r.limit,
                    fmt(lim, d),
                    r.field,
                    fmt(x, nd)
                );
                (vec![x], nd, msg)
            }
            ("multiple_of", _) => {
                let lo = v - v % lim;
                let nearest: Vec<u128> = [lo, lo.saturating_add(lim)].into_iter().filter(|n| *n > 0).collect();
                let list: Vec<String> = nearest.iter().map(|n| fmt(*n, d)).collect();
                let msg = format!(
                    "{label} {} isn't a multiple of {} {} (nearest: {})",
                    fmt(v, d),
                    r.limit,
                    fmt(lim, d),
                    list.join(" or ")
                );
                (nearest, d, msg)
            }
            _ => continue,
        };
        return Err(Error::OrderOutOfBounds(Box::new(OrderOutOfBounds {
            message: format!("{name}: {msg}"),
            instrument: name.to_owned(),
            leg: index,
            field: label,
            check: r.check.to_owned(),
            limit: r.limit.to_owned(),
            value: fmt(v, d),
            limit_value: fmt(lim, d),
            nearest: nearest.iter().map(|n| fmt(*n, nd)).collect(),
            server_code: r.server_code,
        })));
    }
    Ok(())
}

/// Every instrument named by a leg anywhere in an encoded request.
pub fn leg_instruments(body: &[u8], legs: &str, instrument: &str) -> Vec<String> {
    fn walk(v: &Json, legs: &str, instrument: &str, out: &mut Vec<String>) {
        match v {
            Json::Array(xs) => xs.iter().for_each(|x| walk(x, legs, instrument, out)),
            Json::Object(m) => {
                for (k, x) in m {
                    match (k == legs, x.as_array()) {
                        (true, Some(list)) => {
                            for name in list.iter().filter_map(|l| l.get(instrument).and_then(Json::as_str)) {
                                if !name.is_empty() && !out.iter().any(|n| n == name) {
                                    out.push(name.to_owned());
                                }
                            }
                        }
                        _ => walk(x, legs, instrument, out),
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_slice::<Json>(body) {
        walk(&v, legs, instrument, &mut out);
    }
    out
}
