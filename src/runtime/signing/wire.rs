//! Helpers the generated sign functions call: one per contract transform (see
//! sdk/contract/contract.json `signed.transforms`). Errors are `Error::Signing` and always happen
//! before anything is signed.

use std::collections::HashMap;
use std::str::FromStr;

use rust_decimal::Decimal;
use serde::Deserialize;

use sha3::{Digest, Keccak256};

use super::context::{PrepareContext, SigningRequest};
use super::eip712::{grvt_domain, StructType, Value};
use super::keccak256;
use super::signer::Signature;
use crate::runtime::error::{Error, Result};

/// A wire value as the transforms see it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scalar<'a> {
    Str(&'a str),
    Int(i128),
    Bool(bool),
}

/// Model field types the transforms can read.
pub trait ToScalar {
    fn scalar(&self) -> Scalar<'_>;
}

impl ToScalar for String {
    fn scalar(&self) -> Scalar<'_> {
        Scalar::Str(self)
    }
}

impl ToScalar for bool {
    fn scalar(&self) -> Scalar<'_> {
        Scalar::Bool(*self)
    }
}

macro_rules! int_scalar {
    ($($t:ty),*) => {$(
        impl ToScalar for $t {
            fn scalar(&self) -> Scalar<'_> { Scalar::Int(*self as i128) }
        }
    )*};
}
int_scalar!(u8, u16, u32, u64, i8, i16, i32, i64);

/// Generated enums: wire name and the integer used when signing (`ENUM_CODES` in Python).
pub trait WireEnum {
    fn wire_name(&self) -> &str;
    /// `None` for a value this SDK does not know (it decoded from a newer server).
    fn code(&self) -> Option<i64>;
}

fn absent(v: Option<Scalar<'_>>) -> bool {
    matches!(v, None | Some(Scalar::Str("")))
}

/// `copy`, `sig.nonce`, `sig.expiration`: the wire value as the EIP-712 type, or `default` when absent.
pub fn copy<T: ToScalar + ?Sized>(
    value: Option<&T>,
    eip_type: &str,
    at: &str,
    default: Option<Scalar<'_>>,
) -> Result<Value> {
    let mut v = value.map(|x| x.scalar());
    if absent(v) {
        v = Some(default.ok_or_else(|| Error::Signing(format!("{at} is required for signing")))?);
    }
    let v = v.expect("set above");
    match eip_type {
        "bool" => Ok(Value::Bool(match v {
            Scalar::Bool(b) => b,
            Scalar::Str(s) => s.eq_ignore_ascii_case("true"),
            Scalar::Int(n) => n != 0,
        })),
        "address" => match v {
            Scalar::Str(s) => to_address(s, at),
            _ => Err(Error::Signing(format!("{at}: not an address"))),
        },
        "string" => match v {
            Scalar::Str(s) => Ok(Value::String(s.to_owned())),
            _ => Err(Error::Signing(format!("{at}: not a string"))),
        },
        t if t.starts_with("uint") || t.starts_with("int") => match v {
            Scalar::Int(n) => Ok(Value::Int(n)),
            Scalar::Str(s) => to_int(s, at),
            Scalar::Bool(b) => Ok(Value::Int(b as i128)),
        },
        t => Err(Error::Signing(format!("{at}: unsupported copy type {t}"))),
    }
}

/// `address`: absent is the zero address.
pub fn address(value: Option<&String>, at: &str) -> Result<Value> {
    match value.map(String::as_str) {
        None | Some("") => Ok(Value::Address([0u8; 20])),
        Some(s) => to_address(s, at),
    }
}

/// Builder-style variant rules: is this address set to something other than zero?
pub fn address_set(value: Option<&String>) -> bool {
    match value.map(String::as_str) {
        None | Some("") => false,
        Some(s) => {
            let h = strip_0x(s).as_bytes();
            h.len().is_multiple_of(2) && h.iter().all(|c| nibble(*c).is_some()) && h.iter().any(|c| *c != b'0')
        }
    }
}

/// `enumCode`: the enum's signed integer. Absent is 0; a value this SDK does not know is an error.
pub fn enum_code<E: WireEnum>(value: Option<&E>, at: &str) -> Result<Value> {
    match value {
        None => Ok(Value::Int(0)),
        Some(e) => e
            .code()
            .map(|c| Value::Int(c as i128))
            .ok_or_else(|| Error::Signing(format!("{at}: unknown enum value {:?}", e.wire_name()))),
    }
}

/// A field the `scaled` transform reads: a decimal string, or a `Decimal` model field (read as the
/// string it encodes to, without building it).
#[doc(hidden)]
pub trait ScaledValue {
    /// Absent: falls back to the transform's default.
    fn absent(&self) -> bool;
    fn scale(&self, decimals: u32) -> std::result::Result<i128, String>;
}

impl ScaledValue for String {
    fn absent(&self) -> bool {
        self.is_empty()
    }
    fn scale(&self, decimals: u32) -> std::result::Result<i128, String> {
        scale_decimal(self, decimals)
    }
}

impl ScaledValue for Decimal {
    fn absent(&self) -> bool {
        false
    }
    fn scale(&self, decimals: u32) -> std::result::Result<i128, String> {
        scale_decimal_value(*self, decimals)
    }
}

/// `scaled`: decimal × 10^decimals. Over-precision is rejected, never rounded.
pub fn scaled<T: ScaledValue + ?Sized>(
    value: Option<&T>,
    decimals: u32,
    at: &str,
    default: Option<&str>,
) -> Result<Value> {
    let n = match value.filter(|v| !v.absent()) {
        Some(v) => v.scale(decimals),
        None => scale_decimal(
            default.ok_or_else(|| Error::Signing(format!("{at} is required for signing")))?,
            decimals,
        ),
    };
    n.map(Value::Int).map_err(|e| Error::Signing(format!("{at}: {e}")))
}

/// "65000.1", 9 -> 65000100000000.
pub fn scale_decimal(s: &str, decimals: u32) -> std::result::Result<i128, String> {
    let t = s.trim();
    let d = match crate::runtime::codec::parse_plain_decimal(t.as_bytes()) {
        Some(d) => d,
        None => Decimal::from_str(t).map_err(|_| format!("not a decimal: {s:?}"))?,
    };
    scale_parsed(d, decimals).map_err(|why| why.message(s))
}

/// `scale_decimal` of the decimal's string form, without building the string unless it fails.
pub(crate) fn scale_decimal_value(d: Decimal, decimals: u32) -> std::result::Result<i128, String> {
    scale_parsed(d, decimals).map_err(|why| why.message(&d.to_string()))
}

enum ScaleError {
    Places(u32),
    TooLarge,
}

impl ScaleError {
    fn message(self, s: &str) -> String {
        match self {
            ScaleError::Places(decimals) => format!("{s} has more than {decimals} decimal places"),
            ScaleError::TooLarge => format!("{s} is too large"),
        }
    }
}

fn scale_parsed(d: Decimal, decimals: u32) -> std::result::Result<i128, ScaleError> {
    let d = d.normalize();
    if d.scale() > decimals {
        return Err(ScaleError::Places(decimals));
    }
    let shift = decimals - d.scale();
    10i128
        .checked_pow(shift)
        .and_then(|p| d.mantissa().checked_mul(p))
        .ok_or(ScaleError::TooLarge)
}

fn to_int(s: &str, at: &str) -> Result<Value> {
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return hex_right::<32>(h)
            .map(Value::U256)
            .ok_or_else(|| Error::Signing(format!("{at}: not a hex integer: {s:?}")));
    }
    s.parse::<i128>()
        .map(Value::Int)
        .map_err(|_| Error::Signing(format!("{at}: not an integer: {s:?}")))
}

fn to_address(s: &str, at: &str) -> Result<Value> {
    let h = strip_0x(s);
    (h.len() == 40)
        .then(|| hex_right::<20>(h))
        .flatten()
        .map(Value::Address)
        .ok_or_else(|| Error::Signing(format!("{at}: not an address: {s:?}")))
}

fn strip_0x(s: &str) -> &str {
    s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s)
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Hex digits (no 0x) right-aligned in N bytes, odd length allowed, without allocating.
fn hex_right<const N: usize>(h: &str) -> Option<[u8; N]> {
    let b = h.as_bytes();
    if b.len() > 2 * N {
        return None;
    }
    let mut out = [0u8; N];
    let mut pos = N;
    let mut i = b.len();
    while i > 0 {
        let lo = nibble(b[i - 1])?;
        let hi = if i >= 2 { nibble(b[i - 2])? } else { 0 };
        pos -= 1;
        out[pos] = (hi << 4) | lo;
        i = i.saturating_sub(2);
    }
    Some(out)
}

// -- generated encoders --------------------------------------------------------------------------
//
// The generated `hash_*` functions fill one fixed buffer per struct with these writers. The range
// checks and messages match the reference encoder the tests keep (tests/reference).

/// `range` names the type in an out-of-range message, `enc` in a does-not-fit one (as the reference does).
fn put_error(
    v: &Value,
    range: Option<std::fmt::Arguments<'_>>,
    enc: std::fmt::Arguments<'_>,
    st: &str,
    field: &str,
) -> Error {
    match (v, range) {
        (Value::Int(n), Some(r)) => Error::Signing(format!("{st}.{field}: value {n} out of range for {r}")),
        (other, _) => Error::Signing(format!("{st}.{field}: value {other:?} does not fit {enc}")),
    }
}

/// `uint<bits>` into a 32-byte word.
#[inline]
pub fn put_uint(word: &mut [u8], v: Value, bits: u32, st: &str, field: &str) -> Result<()> {
    match v {
        Value::Int(n) if n >= 0 && (bits >= 128 || n < 1i128 << bits) => {
            word[16..32].copy_from_slice(&(n as u128).to_be_bytes());
            Ok(())
        }
        Value::U256(w) if 256 - w.iter().take_while(|b| **b == 0).count() as u32 * 8 <= bits => {
            word[..32].copy_from_slice(&w);
            Ok(())
        }
        Value::U256(w) => Err(Error::Signing(format!(
            "{st}.{field}: value {} out of range for uint{bits}",
            super::eip712::hex0x(&w)
        ))),
        v => Err(put_error(
            &v,
            Some(format_args!("uint{bits}")),
            format_args!("Uint({bits})"),
            st,
            field,
        )),
    }
}

/// `int<bits>` (two's complement) into a 32-byte word.
#[inline]
pub fn put_int(word: &mut [u8], v: Value, bits: u32, st: &str, field: &str) -> Result<()> {
    match v {
        Value::Int(n) if bits >= 128 || (-(1i128 << (bits - 1))..1i128 << (bits - 1)).contains(&n) => {
            word[..16].fill(if n < 0 { 0xff } else { 0 });
            word[16..32].copy_from_slice(&n.to_be_bytes());
            Ok(())
        }
        v => Err(put_error(
            &v,
            Some(format_args!("int{bits}")),
            format_args!("Int({bits})"),
            st,
            field,
        )),
    }
}

#[inline]
pub fn put_bool(word: &mut [u8], v: Value, st: &str, field: &str) -> Result<()> {
    match v {
        Value::Bool(b) => {
            word[31] = b as u8;
            Ok(())
        }
        v => Err(put_error(&v, None, format_args!("Bool"), st, field)),
    }
}

#[inline]
pub fn put_address(word: &mut [u8], v: Value, st: &str, field: &str) -> Result<()> {
    match v {
        Value::Address(a) => {
            word[12..32].copy_from_slice(&a);
            Ok(())
        }
        v => Err(put_error(&v, None, format_args!("Address"), st, field)),
    }
}

/// Streaming keccak256 for array items (each item's 32-byte encoding in turn).
#[derive(Default)]
pub struct ListHasher(Keccak256);

impl ListHasher {
    #[inline]
    pub fn update(&mut self, word: &[u8; 32]) {
        self.0.update(word);
    }

    #[inline]
    pub fn finish(self, out: &mut [u8]) {
        out[..32].copy_from_slice(&self.0.finalize());
    }
}

/// keccak256 of a filled struct buffer.
#[inline]
pub fn hash_buf(b: &[u8]) -> [u8; 32] {
    keccak256(b)
}

/// The EIP-712 digest for a struct hash: keccak256(0x1901 || domainSeparator || structHash).
pub fn digest(chain_id: u64, struct_hash: &[u8; 32]) -> [u8; 32] {
    digest_with(&domain_separator(chain_id), struct_hash)
}

pub(crate) fn digest_with(separator: &[u8; 32], struct_hash: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 66];
    buf[0] = 0x19;
    buf[1] = 0x01;
    buf[2..34].copy_from_slice(separator);
    buf[34..].copy_from_slice(struct_hash);
    keccak256(&buf)
}

/// keccak256(EIP712Domain(name "GRVT Exchange", version "0", chainId)). Cached per thread for the last
/// chain id, so signing does not recompute it.
pub fn domain_separator(chain_id: u64) -> [u8; 32] {
    use std::cell::Cell;
    use std::sync::LazyLock;
    thread_local! {
        static LAST: Cell<Option<(u64, [u8; 32])>> = const { Cell::new(None) };
    }
    if let Some((c, sep)) = LAST.with(Cell::get) {
        if c == chain_id {
            return sep;
        }
    }
    // type hash, name hash, version hash of the Grvt domain; only the chain id word varies.
    static PREFIX: LazyLock<[u8; 96]> = LazyLock::new(|| {
        let mut p = [0u8; 96];
        p[..32].copy_from_slice(&keccak256(b"EIP712Domain(string name,string version,uint256 chainId)"));
        p[32..64].copy_from_slice(&keccak256(grvt_domain(0).name.as_bytes()));
        p[64..].copy_from_slice(&keccak256(grvt_domain(0).version.as_bytes()));
        p
    });
    let mut buf = [0u8; 128];
    buf[..96].copy_from_slice(&*PREFIX);
    buf[120..].copy_from_slice(&chain_id.to_be_bytes());
    let sep = keccak256(&buf);
    LAST.with(|l| l.set(Some((chain_id, sep))));
    sep
}

/// The request a generated `prepare` returns for one struct hash.
#[inline]
pub fn signing_request(chain_id: u64, st: &'static StructType, struct_hash: [u8; 32]) -> SigningRequest<'static> {
    SigningRequest::new(chain_id, st, struct_hash)
}

// -- metadata ----------------------------------------------------------------------------------

/// What signing needs from an instrument (from `get_instrument`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct InstrumentMeta {
    pub instrument_hash: String,
    pub base_decimals: u32,
    #[serde(default)]
    pub quote_decimals: u32,
    /// The instrument limits the order checks read (tick_size, min_size, min_notional), as decimal strings.
    /// A limit that is missing or empty is not checked, as in every SDK; `signing::instrument_meta` fills all
    /// three from `get_instrument`, so build this from it rather than by hand.
    #[serde(default)]
    pub limits: HashMap<String, String>,
}

/// What signing needs from a currency (from `get_currency`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CurrencyMeta {
    pub id: u32,
    pub symbol: String,
    pub balance_decimals: u32,
}

/// The metadata one sign call needs, already loaded: instruments, currencies and, for calls that sign
/// it, the session's main account (contract `signed.main_account`).
#[derive(Debug, Default, Clone)]
pub struct Meta {
    pub instruments: HashMap<String, InstrumentMeta>,
    pub currencies: HashMap<String, CurrencyMeta>,
    pub main_account: Option<String>,
}

impl Meta {
    pub fn instrument(&self, name: Option<&String>) -> Result<&InstrumentMeta> {
        self.instrument_named(name.map(String::as_str).unwrap_or(""))
    }

    pub(crate) fn instrument_named(&self, name: &str) -> Result<&InstrumentMeta> {
        self.instruments
            .get(name)
            .ok_or_else(|| Error::Signing(format!("no metadata for instrument {name:?}")))
    }

    pub fn currency(&self, symbol: Option<&String>) -> Result<&CurrencyMeta> {
        let symbol = symbol.map(String::as_str).unwrap_or("");
        self.currencies
            .get(symbol)
            .ok_or_else(|| Error::Signing(format!("no metadata for currency {symbol:?}")))
    }

    /// `assetKey`: the instrument hash as a uint256.
    pub fn asset_key(&self, name: Option<&String>) -> Result<Value> {
        to_int(&self.instrument(name)?.instrument_hash, "instrument_hash")
    }

    /// `mainAccount`: the session's main account address.
    pub fn main_account(&self) -> Result<Value> {
        let a = self
            .main_account
            .as_deref()
            .ok_or_else(|| Error::Signing("no main account loaded".into()))?;
        to_address(a, "main account")
    }

    /// `currencyId`.
    pub fn currency_id(&self, symbol: Option<&String>) -> Result<Value> {
        Ok(Value::Int(self.currency(symbol)?.id as i128))
    }
}

/// The non-empty names among `values`, deduplicated in order.
pub fn present<'a>(values: impl IntoIterator<Item = Option<&'a String>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in values.into_iter().flatten() {
        if !v.is_empty() && !out.contains(v) {
            out.push(v.clone());
        }
    }
    out
}

/// The items of an optional list (an `each` mapping).
pub fn each<T>(list: Option<&Vec<T>>) -> impl Iterator<Item = &T> {
    list.into_iter().flatten()
}

// -- signature object ----------------------------------------------------------------------------

/// Fill signer, nonce, expiration and chain_id when absent (expiration 0 counts as absent) and return
/// the chain id of the EIP-712 domain: the signature object's own chain id when set and not 0, else the
/// context's.
pub fn prepare_signature(
    ctx: &PrepareContext<'_>,
    signer: &mut Option<String>,
    nonce: &mut Option<u32>,
    expiration: &mut Option<crate::Timestamp>,
    chain_id: &mut Option<u64>,
    ttl_ns: i64,
) -> Result<u64> {
    if signer.as_deref().is_none_or(str::is_empty) {
        *signer = Some(ctx.address.to_owned());
    }
    if nonce.is_none() {
        *nonce = Some(ctx.next_nonce()?);
    }
    if expiration.is_none_or(|e| e.nanoseconds() == 0) {
        *expiration = Some(crate::Timestamp::from_nanoseconds(ctx.now_ns() + ttl_ns));
    }
    if chain_id.is_none() {
        *chain_id = Some(ctx.chain_id);
    }
    match *chain_id {
        Some(c) if c != 0 => Ok(c),
        _ if ctx.chain_id != 0 => Ok(ctx.chain_id),
        _ => Err(Error::Config(
            "SignContext::chain_id is 0: set the environment's chain id".into(),
        )),
    }
}

/// Whether a signature object has everything `prepare_signature` fills.
pub fn is_prepared(
    signer: &Option<String>,
    nonce: Option<u32>,
    expiration: Option<crate::Timestamp>,
    chain_id: Option<u64>,
) -> bool {
    signer.as_deref().is_some_and(|s| !s.is_empty())
        && nonce.is_some()
        && expiration.is_some_and(|e| e.nanoseconds() != 0)
        && chain_id.is_some()
}

/// Write a signature into a signature object. An empty signer field keeps the one already there.
pub fn write_signature(
    signer: &mut Option<String>,
    r: &mut Option<String>,
    s: &mut Option<String>,
    v: &mut Option<u8>,
    sig: Signature,
) {
    if !sig.signer.is_empty() {
        *signer = Some(sig.signer);
    }
    *r = Some(sig.r);
    *s = Some(sig.s);
    *v = Some(sig.v);
}

/// `<primary joined by ->[@<secondary joined by ->]`, skipping fields left unset.
pub fn selector(primary: &[Option<String>], secondary: &[Option<String>]) -> String {
    let join = |xs: &[Option<String>]| {
        xs.iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("-")
    };
    let p = join(primary);
    let s = join(secondary);
    if s.is_empty() {
        p
    } else {
        format!("{p}@{s}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fast path in `scale_decimal` scales exactly as `Decimal::from_str` did, results and errors.
    #[test]
    fn scaling_fast_path_matches_from_str() {
        fn reference(s: &str, decimals: u32) -> std::result::Result<i128, String> {
            let d = Decimal::from_str(s.trim())
                .map_err(|_| format!("not a decimal: {s:?}"))?
                .normalize();
            if d.scale() > decimals {
                return Err(format!("{s} has more than {decimals} decimal places"));
            }
            10i128
                .checked_pow(decimals - d.scale())
                .and_then(|p| d.mantissa().checked_mul(p))
                .ok_or_else(|| format!("{s} is too large"))
        }
        for s in crate::runtime::codec::tests::decimal_inputs() {
            for decimals in [0, 4, 9, 18, 30, 40] {
                assert_eq!(scale_decimal(&s, decimals), reference(&s, decimals), "{s:?} {decimals}");
            }
            if let Ok(d) = Decimal::from_str_exact(&s) {
                assert_eq!(scale_decimal_value(d, 9), reference(&d.to_string(), 9), "{s:?}");
            }
        }
    }

    #[test]
    fn scaling_rejects_over_precision() {
        assert_eq!(scale_decimal("65000.1", 9), Ok(65_000_100_000_000));
        assert_eq!(scale_decimal("1.500", 1), Ok(15));
        assert_eq!(scale_decimal("0", 9), Ok(0));
        assert!(scale_decimal("1.1234567891", 9).unwrap_err().contains("decimal places"));
        assert!(scale_decimal("abc", 9).is_err());
        assert_eq!(scale_decimal("-100.25", 6), Ok(-100_250_000));
    }

    #[test]
    fn copy_and_address() {
        let n = "8289849667772468".to_string();
        assert_eq!(
            copy(Some(&n), "uint64", "x", None).unwrap(),
            Value::Int(8289849667772468)
        );
        assert_eq!(
            copy::<String>(None, "bool", "x", Some(Scalar::Bool(false))).unwrap(),
            Value::Bool(false)
        );
        assert!(copy::<String>(None, "uint64", "x", None).is_err());
        assert!(!address_set(Some(
            &"0x0000000000000000000000000000000000000000".to_string()
        )));
        assert!(address_set(Some(
            &"0x00000000000000000000000000000000000000a1".to_string()
        )));
        assert_eq!(
            selector(&[Some("BTC".into()), None], &[Some("500".into()), Some("10".into())]),
            "BTC@500-10"
        );
        assert_eq!(selector(&[Some("BTC".into())], &[None]), "BTC");
    }
}
