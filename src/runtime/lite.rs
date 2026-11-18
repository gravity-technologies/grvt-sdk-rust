//! The lite wire encoding: the same API with short JSON field names (`instrument` -> `i`).
//!
//! Lite names are unique only within one type (`s` is a frame's stream, a book level's size and a
//! signature's signer), so every type has its own lite codec, generated into `generated/lite.rs` as a serde
//! mirror of the model with lite names. A mirror decodes straight into the model and encodes from it, so
//! a lite decode costs the same as a full one. Models, signing and errors only ever see full names: lite
//! exists only between the socket or HTTP body and the model.
//!
//! - `LiteEncode` / `LiteDecode`: implemented by the generated code for every model that is sent or
//!   received (edge models, which have no lite form, use their full codec).
//! - `Encoding`: the client option, with the encode/decode entry points the runtime uses.

use std::fmt;
use std::marker::PhantomData;

use serde::de::{DeserializeOwned, Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// Which JSON field names a client sends and reads. One setting covers every gateway call and stream of
/// a client; edge login and `GET /time` always use full names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Encoding {
    /// Full field names (`/full/v1/...`, `/ws/full`).
    Full,
    /// Short field names (`/lite/v1/...`, `/ws/lite`): smaller frames, same calls and values. The default.
    #[default]
    Lite,
}

impl Encoding {
    /// `"full"` or `"lite"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Full => "full",
            Encoding::Lite => "lite",
        }
    }

    /// Append `value`'s JSON in this encoding to `out`.
    pub fn write<T: Serialize + LiteEncode + ?Sized>(self, value: &T, out: &mut Vec<u8>) -> serde_json::Result<()> {
        match self {
            Encoding::Full => serde_json::to_writer(out, value),
            Encoding::Lite => serde_json::to_writer(out, &AsLite(value)),
        }
    }

    /// `value`'s JSON in this encoding.
    pub fn to_vec<T: Serialize + LiteEncode + ?Sized>(self, value: &T) -> serde_json::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(128);
        self.write(value, &mut out)?;
        Ok(out)
    }

    /// Decode JSON in this encoding. Unknown keys are skipped in both encodings.
    pub fn from_slice<T: DeserializeOwned + LiteDecode>(self, json: &[u8]) -> serde_json::Result<T> {
        match self {
            Encoding::Full => serde_json::from_slice(json),
            Encoding::Lite => {
                let mut de = serde_json::Deserializer::from_slice(json);
                let value = T::decode_lite(&mut de)?;
                de.end()?;
                Ok(value)
            }
        }
    }

    /// `from_slice` for a value that may borrow from `json`.
    pub fn from_str<'a, T: Deserialize<'a> + LiteDecode>(self, json: &'a str) -> serde_json::Result<T> {
        match self {
            Encoding::Full => serde_json::from_str(json),
            Encoding::Lite => {
                let mut de = serde_json::Deserializer::from_str(json);
                let value = T::decode_lite(&mut de)?;
                de.end()?;
                Ok(value)
            }
        }
    }
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A model's lite encoder (generated).
pub trait LiteEncode {
    fn encode_lite<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error>;
}

/// A model's lite decoder (generated).
pub trait LiteDecode: Sized {
    fn decode_lite<'de, D: Deserializer<'de>>(d: D) -> Result<Self, D::Error>;
}

/// Serializes the wrapped value with lite names.
pub struct AsLite<'a, T: ?Sized>(pub &'a T);

impl<T: LiteEncode + ?Sized> Serialize for AsLite<'_, T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.encode_lite(s)
    }
}

/// Deserializes a value from lite names.
pub struct FromLite<T>(pub T);

impl<'de, T: LiteDecode> Deserialize<'de> for FromLite<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        T::decode_lite(d).map(FromLite)
    }
}

impl<T: LiteEncode> LiteEncode for Vec<T> {
    fn encode_lite<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.iter().map(AsLite))
    }
}

impl<T: LiteEncode> LiteEncode for Option<T> {
    fn encode_lite<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Some(v) => s.serialize_some(&AsLite(v)),
            None => s.serialize_none(),
        }
    }
}

impl<T: LiteEncode + ?Sized> LiteEncode for Box<T> {
    fn encode_lite<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        (**self).encode_lite(s)
    }
}

impl<T: LiteDecode> LiteDecode for Vec<T> {
    fn decode_lite<'de, D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: LiteDecode> Visitor<'de> for V<T> {
            type Value = Vec<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a list")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
                // serde's own cap on a size hint, so a bad hint cannot reserve a huge buffer
                let cap = seq
                    .size_hint()
                    .unwrap_or(0)
                    .min(1024 * 1024 / std::mem::size_of::<T>().max(1));
                let mut out = Vec::with_capacity(cap);
                while let Some(FromLite(v)) = seq.next_element::<FromLite<T>>()? {
                    out.push(v);
                }
                Ok(out)
            }
        }
        d.deserialize_seq(V(PhantomData))
    }
}

impl<T: LiteDecode> LiteDecode for Option<T> {
    fn decode_lite<'de, D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Option::<FromLite<T>>::deserialize(d).map(|o| o.map(|v| v.0))
    }
}

impl<T: LiteDecode> LiteDecode for Box<T> {
    fn decode_lite<'de, D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        T::decode_lite(d).map(Box::new)
    }
}

/// serde `with` module for a model field that holds other models: encodes and decodes them in lite.
#[doc(hidden)]
pub mod nested {
    use super::{LiteDecode, LiteEncode};
    use serde::{Deserializer, Serializer};

    pub fn serialize<T: LiteEncode, S: Serializer>(v: &T, s: S) -> Result<S::Ok, S::Error> {
        v.encode_lite(s)
    }

    pub fn deserialize<'de, T: LiteDecode, D: Deserializer<'de>>(d: D) -> Result<T, D::Error> {
        T::decode_lite(d)
    }
}

/// `LiteEncode` / `LiteDecode` for a model through its generated mirror (`remote` serde derive).
#[doc(hidden)]
#[macro_export]
macro_rules! lite_codec {
    ($model:ty, $mirror:ident, encode) => {
        impl $crate::runtime::lite::LiteEncode for $model {
            #[inline]
            fn encode_lite<S: ::serde::Serializer>(&self, s: S) -> ::std::result::Result<S::Ok, S::Error> {
                $mirror::serialize(self, s)
            }
        }
    };
    ($model:ty, $mirror:ident, decode) => {
        impl $crate::runtime::lite::LiteDecode for $model {
            #[inline]
            fn decode_lite<'de, D: ::serde::Deserializer<'de>>(d: D) -> ::std::result::Result<Self, D::Error> {
                $mirror::deserialize(d)
            }
        }
    };
    ($model:ty, $mirror:ident, encode, decode) => {
        $crate::lite_codec!($model, $mirror, encode);
        $crate::lite_codec!($model, $mirror, decode);
    };
}

/// `LiteEncode` / `LiteDecode` as the full codec, for models that have no lite form (edge login).
#[doc(hidden)]
#[macro_export]
macro_rules! full_codec {
    ($model:ty) => {
        impl $crate::runtime::lite::LiteEncode for $model {
            fn encode_lite<S: ::serde::Serializer>(&self, s: S) -> ::std::result::Result<S::Ok, S::Error> {
                ::serde::Serialize::serialize(self, s)
            }
        }
        impl $crate::runtime::lite::LiteDecode for $model {
            fn decode_lite<'de, D: ::serde::Deserializer<'de>>(d: D) -> ::std::result::Result<Self, D::Error> {
                <Self as ::serde::Deserialize<'de>>::deserialize(d)
            }
        }
    };
}
