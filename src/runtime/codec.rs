//! JSON wire rules and the WS frame envelope. Field names are full; the few hand-written lite structs
//! (`LiteErrorBody`, `LiteSubscribeResult`) mirror their full ones (models get generated lite codecs).
//!
//! - Contract decimal fields use `rust_decimal::Decimal` and travel as JSON strings.
//!   Decoding rejects excess precision instead of rounding.
//! - 64-bit integers are JSON strings ("1793172225200080949"): the gateway rejects them as JSON
//!   numbers. Models hold `u64`/`i64` and use [`opt_u64`]/[`opt_i64`] (decode also accepts a number
//!   or a 0x hex string, encode always writes a decimal string).
//! - Small integers (uint8/16/32) are JSON numbers; decode also accepts a quoted number ([`opt_num`]).
//! - Unknown fields are ignored on decode.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serializer};
use serde_json::value::RawValue;

/// Visitor for an integer sent as a number, a decimal string or a 0x hex string. `""` is `None`.
struct IntVisitor<T>(PhantomData<T>);

impl<'de, T> Visitor<'de> for IntVisitor<T>
where
    T: TryFrom<u64> + TryFrom<i64> + TryFrom<u128> + FromStr,
{
    type Value = Option<T>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an integer, a quoted integer or null")
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        T::try_from(v)
            .map(Some)
            .map_err(|_| E::custom(format!("{v} out of range")))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        T::try_from(v)
            .map(Some)
            .map_err(|_| E::custom(format!("{v} out of range")))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        if v.fract() == 0.0 && v.abs() < 9.0e15 {
            return self.visit_i64(v as i64);
        }
        Err(E::custom(format!("{v} is not an integer")))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        let s = v.trim();
        if s.is_empty() {
            return Ok(None);
        }
        if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            let n = u128::from_str_radix(hex, 16).map_err(|e| E::custom(format!("{v:?}: {e}")))?;
            return T::try_from(n)
                .map(Some)
                .map_err(|_| E::custom(format!("{v} out of range")));
        }
        s.parse::<T>()
            .map(Some)
            .map_err(|_| E::custom(format!("{v:?} is not an integer")))
    }
}

fn de_int<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: TryFrom<u64> + TryFrom<i64> + TryFrom<u128> + FromStr,
{
    d.deserialize_option(IntVisitor::<T>(PhantomData))
}

macro_rules! wide_int {
    ($module:ident, $t:ty) => {
        #[doc = concat!("`Option<", stringify!($t), ">` as a JSON string.")]
        pub mod $module {
            use super::*;

            pub fn serialize<S: Serializer>(v: &Option<$t>, s: S) -> Result<S::Ok, S::Error> {
                match v {
                    Some(n) => s.collect_str(n),
                    None => s.serialize_none(),
                }
            }

            pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<$t>, D::Error> {
                de_int::<D, $t>(d)
            }
        }
    };
}

wide_int!(opt_u64, u64);
wide_int!(opt_i64, i64);

/// `Option<Vec<u64>>` as a JSON array of strings.
pub mod opt_vec_u64 {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Option<Vec<u64>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(items) => s.collect_seq(items.iter().map(|n| n.to_string())),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u64>>, D::Error> {
        Option::<LenientVec<u64>>::deserialize(d).map(|o| o.map(|v| v.0))
    }
}

/// Small integers: written as JSON numbers, read from a number or a quoted number.
pub mod opt_num {
    use super::*;

    pub fn serialize<S: Serializer, T: serde::Serialize>(v: &Option<T>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(n) => n.serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: TryFrom<u64> + TryFrom<i64> + TryFrom<u128> + FromStr,
    {
        de_int::<D, T>(d)
    }
}

/// A list of integers, each a number or a quoted number.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LenientVec<T>(pub Vec<T>);

impl<'de, T> Deserialize<'de> for LenientVec<T>
where
    T: TryFrom<u64> + TryFrom<i64> + TryFrom<u128> + FromStr,
{
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T> Visitor<'de> for V<T>
        where
            T: TryFrom<u64> + TryFrom<i64> + TryFrom<u128> + FromStr,
        {
            type Value = LenientVec<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a list of integers")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(item) = seq.next_element::<Lenient<T>>()? {
                    if let Some(v) = item.0 {
                        out.push(v);
                    }
                }
                Ok(LenientVec(out))
            }
        }
        d.deserialize_seq(V(PhantomData))
    }
}

struct Lenient<T>(Option<T>);

impl<'de, T> Deserialize<'de> for Lenient<T>
where
    T: TryFrom<u64> + TryFrom<i64> + TryFrom<u128> + FromStr,
{
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        de_int::<D, T>(d).map(Lenient)
    }
}

/// `Option<Cow<str>>` that borrows from the input when the string has no escapes (serde's own `Cow`
/// impl always copies inside an `Option`).
pub fn opt_cow_str<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Cow<'de, str>>, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<Cow<'de, str>>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a string or null")
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_str(self)
        }
        fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
            Ok(Some(Cow::Borrowed(v)))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(Cow::Owned(v.to_owned())))
        }
    }
    d.deserialize_option(V)
}

/// `{"code": ..., "message": ...}` as sent by the gateway (REST error body, WS `error`).
#[derive(Debug, Default, Deserialize)]
pub struct ErrorBody {
    pub code: u32,
    pub message: String,
}

/// `ErrorBody` with lite names (`Error` `c m`; the lite HTTP error body adds `s`, `ri`, ignored here).
#[derive(Debug, Default, Deserialize)]
pub struct LiteErrorBody {
    #[serde(rename = "c")]
    pub code: u32,
    #[serde(rename = "m")]
    pub message: String,
}

impl From<LiteErrorBody> for ErrorBody {
    fn from(b: LiteErrorBody) -> Self {
        ErrorBody {
            code: b.code,
            message: b.message,
        }
    }
}

/// The parts of an incoming WS frame the read loop needs. `result` and `feed` stay raw: the read
/// loop only routes, consumers decode.
#[derive(Debug, Deserialize)]
pub struct Frame<'a> {
    #[serde(default, with = "opt_num")]
    pub id: Option<u32>,
    #[serde(borrow, default)]
    pub result: Option<&'a RawValue>,
    #[serde(default)]
    pub error: Option<ErrorBody>,
    #[serde(borrow, default)]
    pub stream: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub selector: Option<Cow<'a, str>>,
    #[serde(default, with = "opt_u64")]
    pub sequence_number: Option<u64>,
    #[serde(default, with = "opt_u64")]
    pub prev_sequence_number: Option<u64>,
    /// REST-style error body (`{"code","message","status"}`) sometimes seen on WS too.
    #[serde(default, with = "opt_num")]
    pub code: Option<u32>,
    #[serde(default)]
    pub message: Option<String>,
}

pub fn decode_frame(data: &[u8]) -> serde_json::Result<Frame<'_>> {
    serde_json::from_slice(data)
}

/// The `result` of a subscribe reply.
#[derive(Debug, Default, Deserialize)]
pub struct SubscribeResult {
    #[serde(default)]
    pub stream: String,
    #[serde(default)]
    pub subs: Vec<String>,
    #[serde(default)]
    pub unsubs: Vec<String>,
    #[serde(default)]
    pub num_snapshots: LenientVec<u64>,
    #[serde(default)]
    pub first_sequence_number: LenientVec<u64>,
    #[serde(default)]
    pub latest_sequence_number: LenientVec<u64>,
}

/// `SubscribeResult` with lite names (`WSSubscribeResult` `s s1 u ns fs ls`).
#[derive(Debug, Default, Deserialize)]
pub struct LiteSubscribeResult {
    #[serde(rename = "s", default)]
    pub stream: String,
    #[serde(rename = "s1", default)]
    pub subs: Vec<String>,
    #[serde(rename = "u", default)]
    pub unsubs: Vec<String>,
    #[serde(rename = "ns", default)]
    pub num_snapshots: LenientVec<u64>,
    #[serde(rename = "fs", default)]
    pub first_sequence_number: LenientVec<u64>,
    #[serde(rename = "ls", default)]
    pub latest_sequence_number: LenientVec<u64>,
}

impl From<LiteSubscribeResult> for SubscribeResult {
    fn from(r: LiteSubscribeResult) -> Self {
        SubscribeResult {
            stream: r.stream,
            subs: r.subs,
            unsubs: r.unsubs,
            num_snapshots: r.num_snapshots,
            first_sequence_number: r.first_sequence_number,
            latest_sequence_number: r.latest_sequence_number,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct M {
        #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_u64")]
        a: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_i64")]
        b: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_num")]
        c: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_vec_u64")]
        d: Option<Vec<u64>>,
    }

    #[test]
    fn wide_ints_are_strings_on_the_wire() {
        let m: M = serde_json::from_str(r#"{"a":"18446744073709551615","b":-5,"c":"7","d":["1",2]}"#).unwrap();
        assert_eq!(
            m,
            M {
                a: Some(u64::MAX),
                b: Some(-5),
                c: Some(7),
                d: Some(vec![1, 2])
            }
        );
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"a":"18446744073709551615","b":"-5","c":7,"d":["1","2"]}"#
        );
        let hex: M = serde_json::from_str(r#"{"a":"0x030501","c":null}"#).unwrap();
        assert_eq!(hex.a, Some(0x030501));
        assert!(serde_json::from_str::<M>(r#"{"c":"300000000000"}"#).is_err());
    }

    /// A bad decimal or timestamp reports the same error, position included, as when it was read into an
    /// owned String first.
    #[test]
    fn decode_errors_keep_their_text() {
        #[derive(Deserialize, Debug)]
        #[allow(dead_code)]
        struct D {
            #[serde(default, with = "opt_decimal")]
            a: Option<rust_decimal::Decimal>,
            b: String,
        }
        #[derive(Deserialize, Debug)]
        #[allow(dead_code)]
        struct T {
            t: crate::Timestamp,
            b: String,
        }
        let d = serde_json::from_str::<D>(r#"{"b":"1","a":"1e5"}"#)
            .unwrap_err()
            .to_string();
        let old = rust_decimal::Decimal::from_str_exact("1e5").unwrap_err();
        assert_eq!(d, format!("{old} at line 1 column 19"));
        let t = serde_json::from_str::<T>(r#"{"b":"1","t":"0."}"#)
            .unwrap_err()
            .to_string();
        let old = "0.".parse::<i64>().unwrap_err();
        assert_eq!(t, format!("{old} at line 1 column 18"));
    }

    /// Edge cases plus seeded random digit strings, with and without a dot and a minus sign.
    pub(crate) fn decimal_inputs() -> Vec<String> {
        let mut inputs: Vec<String> = [
            "0",
            "-0",
            "0.0",
            "-0.00",
            "00",
            "007",
            "1",
            "-1",
            "1.5",
            "65000.5",
            "1.230",
            "0.000000001",
            "-100.25",
            "9999999999999999999",
            "99999999999999999999",
            "1234567890.123456789",
            "0.1234567890123456789",
            "18446744073709551615",
            "9223372036854775807",
            "-9223372036854775808",
            "1.",
            ".5",
            "-.5",
            "+1",
            "1_000",
            "1e5",
            " 1",
            "1 ",
            "",
            "-",
            ".",
            "1.2.3",
            "abc",
            "0.1234567890123456789012345678",
            "0.12345678901234567890123456789",
            "999999999999999999999999999999",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let digits = (x % 21) as usize + 1;
            let mut s: String = (0..digits)
                .map(|i| char::from(b'0' + ((x >> (i * 3)) % 10) as u8))
                .collect();
            if x & 1 == 1 && digits > 1 {
                s.insert((x as usize >> 8) % (digits + 1), '.');
            }
            if x & 2 == 2 {
                s.insert(0, '-');
            }
            inputs.push(s);
        }
        inputs
    }

    #[test]
    fn fast_decimal_parse_matches_from_str_exact() {
        let inputs = decimal_inputs();
        for s in &inputs {
            let fast = parse_decimal(s).map(|d| d.serialize());
            let full = rust_decimal::Decimal::from_str_exact(s).map(|d| d.serialize());
            assert_eq!(fast, full, "{s:?}");
        }
    }

    #[test]
    fn frame_envelope() {
        let f = decode_frame(br#"{"stream":"v1.book.d","selector":"X@500","sequence_number":"7","prev_sequence_number":"6","feed":{"a":1}}"#).unwrap();
        assert_eq!(
            (f.id, f.sequence_number, f.prev_sequence_number),
            (None, Some(7), Some(6))
        );
        let r = decode_frame(br#"{"jsonrpc":"2.0","id":3,"result":{"x":[1,2]}}"#).unwrap();
        assert_eq!(r.result.unwrap().get(), r#"{"x":[1,2]}"#);
    }
}

/// Parse an exact decimal string: `Decimal::from_str_exact`, with a fast path for plain
/// `[-]digits[.digits]` of at most 19 digits (what the wire carries) that builds the same value (same
/// mantissa, scale and sign).
pub(crate) fn parse_decimal(text: &str) -> Result<rust_decimal::Decimal, rust_decimal::Error> {
    match parse_plain_decimal(text.as_bytes()) {
        Some(d) => Ok(d),
        None => rust_decimal::Decimal::from_str_exact(text),
    }
}

pub(crate) fn parse_plain_decimal(b: &[u8]) -> Option<rust_decimal::Decimal> {
    let (negative, digits) = match b.split_first()? {
        (b'-', rest) => (true, rest),
        _ => (false, b),
    };
    // 19 digits always fit u64; a dot needs digits on both sides.
    if digits.is_empty() || digits.len() > 20 {
        return None;
    }
    let (mut mantissa, mut count, mut scale, mut dot) = (0u64, 0u32, 0u32, false);
    for (i, &c) in digits.iter().enumerate() {
        match c {
            b'0'..=b'9' if count < 19 => {
                mantissa = mantissa * 10 + u64::from(c - b'0');
                count += 1;
                scale += u32::from(dot);
            }
            b'.' if !dot && i > 0 && i + 1 < digits.len() => dot = true,
            _ => return None,
        }
    }
    // "-0" and "-0.00" keep their sign bit only through the full parser.
    if negative && mantissa == 0 {
        return None;
    }
    let m = i128::from(mantissa);
    Some(rust_decimal::Decimal::from_i128_with_scale(
        if negative { -m } else { m },
        scale,
    ))
}

/// A string borrowed from the input when it has no escapes. Values are parsed from it after it is read,
/// so a parse error reports the position after the string, as reading an owned `String` did.
pub(crate) struct StrElem<'de>(pub(crate) Cow<'de, str>);

impl<'de> Deserialize<'de> for StrElem<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Cow<'de, str>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(Cow::Borrowed(v))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(Cow::Owned(v.to_owned()))
            }
        }
        d.deserialize_str(V).map(StrElem)
    }
}

/// Parse exact decimal strings. Reject excess precision instead of rounding it.
pub mod opt_decimal {
    use rust_decimal::Decimal;
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<Decimal>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.collect_str(value),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Decimal>, D::Error> {
        Option::<super::StrElem<'de>>::deserialize(deserializer)?
            .map(|text| super::parse_decimal(&text.0).map_err(D::Error::custom))
            .transpose()
    }
}

/// Lists follow the same exact decimal decoding rule as scalar fields.
pub mod opt_vec_decimal {
    use rust_decimal::Decimal;
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<Vec<Decimal>>, serializer: S) -> Result<S::Ok, S::Error> {
        struct AsStr<'a>(&'a Decimal);
        impl serde::Serialize for AsStr<'_> {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self.0)
            }
        }
        match value {
            Some(items) => serializer.collect_seq(items.iter().map(AsStr)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Vec<Decimal>>, D::Error> {
        // Every item is read before any is parsed, so an error reports the same position as before.
        let value = Option::<Vec<super::StrElem<'de>>>::deserialize(deserializer)?;
        value
            .map(|items| {
                items
                    .iter()
                    .map(|text| super::parse_decimal(&text.0).map_err(D::Error::custom))
                    .collect()
            })
            .transpose()
    }
}

/// A nullable value whose key must still be present in a complete model.
pub fn required_nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}

macro_rules! required_scalar {
    ($module:ident, $optional:ident, $t:ty) => {
        pub mod $module {
            use super::*;
            pub fn serialize<S: Serializer>(v: &$t, s: S) -> Result<S::Ok, S::Error> {
                $optional::serialize(&Some(*v), s)
            }
            pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<$t, D::Error> {
                $optional::deserialize(d)?.ok_or_else(|| de::Error::custom("required value cannot be null or empty"))
            }
        }
    };
}
required_scalar!(required_u64, opt_u64, u64);
required_scalar!(required_i64, opt_i64, i64);
required_scalar!(required_decimal, opt_decimal, rust_decimal::Decimal);

pub mod required_num {
    use super::*;
    pub fn serialize<S: Serializer, T: serde::Serialize>(v: &T, s: S) -> Result<S::Ok, S::Error> {
        v.serialize(s)
    }
    pub fn deserialize<'de, D, T>(d: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: TryFrom<u64> + TryFrom<i64> + TryFrom<u128> + FromStr,
    {
        de_int::<D, T>(d)?.ok_or_else(|| de::Error::custom("required value cannot be null or empty"))
    }
}
