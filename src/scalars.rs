//! Exact Unix timestamps. Decimal model fields use `rust_decimal::Decimal`.

use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Unix nanoseconds, retaining all digits from the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    pub const fn from_nanoseconds(value: i64) -> Self {
        Self(value)
    }

    pub const fn nanoseconds(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Parsed after the string is read, so an error reports the position after it.
        let text = crate::runtime::codec::StrElem::deserialize(deserializer)?;
        text.0.parse::<i64>().map(Self).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::Timestamp;

    #[test]
    fn exact_roundtrip() {
        for ns in [i64::MIN, -1, 0, 1_793_172_225_200_080_949, i64::MAX] {
            let value = Timestamp::from_nanoseconds(ns);
            let wire = serde_json::to_string(&value).unwrap();
            assert_eq!(wire, format!("\"{ns}\""));
            assert_eq!(serde_json::from_str::<Timestamp>(&wire).unwrap(), value);
        }
    }

    #[test]
    fn invalid_wire() {
        for wire in ["null", "1", "\"1.5\"", "\"9223372036854775808\""] {
            assert!(serde_json::from_str::<Timestamp>(wire).is_err());
        }
    }
}

#[cfg(test)]
mod model_tests {
    #[test]
    fn decimals_reject_rounding_and_invalid_input() {
        use crate::models::{OrderLeg, OrderState};
        let value: OrderLeg = serde_json::from_str(
            r#"{"instrument":"BTC_USDT_Perp","is_buying_asset":false,"size":"0.1234567890123456789012345678"}"#,
        )
        .unwrap();
        assert_eq!(value.size.to_string(), "0.1234567890123456789012345678");
        for text in [
            "abc",
            "1.2.3",
            "0.12345678901234567890123456789",
            "999999999999999999999999999999",
        ] {
            let array_raw = format!("{{\"book_size\":[\"{text}\"]}}");
            assert!(serde_json::from_str::<OrderState>(&array_raw).is_err());
            let raw = format!("{{\"size\":\"{text}\"}}");
            assert!(
                serde_json::from_str::<OrderLeg>(&raw).is_err(),
                "{text} was rounded or accepted"
            );
        }
    }
}
