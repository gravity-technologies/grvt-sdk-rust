//! EIP-712 typed data: values, the Grvt domain, and the signed struct types as wallet/KMS signers
//! see them. Hashing is generated per message (`hash_*` in generated/signing.rs); there is no generic
//! encoder in the runtime.

use std::collections::BTreeSet;

use serde_json::json;

use super::keccak256;
use crate::runtime::error::{Error, Result};

/// An EIP-712 value. Integers are checked against the declared type when hashed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i128),
    /// A 256-bit unsigned integer, big-endian (e.g. an asset id from a hex instrument hash).
    U256([u8; 32]),
    Bool(bool),
    Address([u8; 20]),
    String(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Struct(Vec<Value>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Domain {
    pub name: String,
    pub version: String,
    pub chain_id: u64,
    pub verifying_contract: Option<[u8; 20]>,
    separator: [u8; 32],
}

impl Domain {
    pub fn new(name: &str, version: &str, chain_id: u64, verifying_contract: Option<[u8; 20]>) -> Domain {
        let mut type_str = String::from("EIP712Domain(string name,string version,uint256 chainId");
        if verifying_contract.is_some() {
            type_str.push_str(",address verifyingContract");
        }
        type_str.push(')');
        let mut buf = Vec::with_capacity(32 * 5);
        buf.extend_from_slice(&keccak256(type_str.as_bytes()));
        buf.extend_from_slice(&keccak256(name.as_bytes()));
        buf.extend_from_slice(&keccak256(version.as_bytes()));
        buf.extend_from_slice(&u128_word(chain_id as u128));
        if let Some(a) = verifying_contract {
            buf.extend_from_slice(&[0u8; 12]);
            buf.extend_from_slice(&a);
        }
        let separator = keccak256(&buf);
        Domain {
            name: name.into(),
            version: version.into(),
            chain_id,
            verifying_contract,
            separator,
        }
    }

    pub fn separator(&self) -> [u8; 32] {
        self.separator
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut d = json!({"name": self.name, "version": self.version, "chainId": self.chain_id.to_string()});
        if let Some(a) = self.verifying_contract {
            d["verifyingContract"] = json!(checksum_address(&a));
        }
        d
    }

    fn types_json(&self) -> serde_json::Value {
        let mut t = vec![
            json!({"name": "name", "type": "string"}),
            json!({"name": "version", "type": "string"}),
            json!({"name": "chainId", "type": "uint256"}),
        ];
        if self.verifying_contract.is_some() {
            t.push(json!({"name": "verifyingContract", "type": "address"}));
        }
        serde_json::Value::Array(t)
    }
}

/// Grvt's domain. The chain id comes from the environment.
pub fn grvt_domain(chain_id: u64) -> Domain {
    Domain::new("GRVT Exchange", "0", chain_id, None)
}

/// One struct definition: name and `(field name, EIP-712 type)` pairs.
pub type TypeDef = (&'static str, &'static [(&'static str, &'static str)]);

/// One of Grvt's signed EIP-712 types (the generated `signing::EIP712_*` statics): the type
/// definitions in eth_signTypedData order. It describes the typed message for wallet/KMS signers;
/// hashing is done by the generated encoders, so there is no generic hashing here.
#[derive(Debug, Clone)]
pub struct StructType {
    pub name: &'static str,
    pub types: &'static [TypeDef],
    type_hash: [u8; 32],
}

impl StructType {
    /// Generated code only: checks the primary type's hash against `expected_type_hash` (0x hex).
    pub(crate) fn new(name: &'static str, types: &'static [TypeDef], expected_type_hash: &str) -> Result<StructType> {
        if !types.iter().any(|(t, _)| *t == name) {
            return Err(Error::Signing(format!("{name}: primary type not defined")));
        }
        let type_hash = keccak256(encode_type(name, types).as_bytes());
        let got = hex0x(&type_hash);
        if !got.eq_ignore_ascii_case(expected_type_hash) {
            return Err(Error::Signing(format!(
                "{name}: type hash mismatch, generated {expected_type_hash} vs computed {got}"
            )));
        }
        Ok(StructType { name, types, type_hash })
    }

    pub fn type_hash(&self) -> [u8; 32] {
        self.type_hash
    }

    pub fn encode_type(&self) -> String {
        encode_type(self.name, self.types)
    }

    /// Standard eth_signTypedData_v4 JSON, for signers that need the full struct (KMS, wallets).
    pub fn to_typed_data(&self, domain: &Domain, message: &Value) -> Result<serde_json::Value> {
        let mut types = serde_json::Map::new();
        types.insert("EIP712Domain".into(), domain.types_json());
        for (n, fields) in self.types {
            let fs: Vec<_> = fields.iter().map(|(f, t)| json!({"name": f, "type": t})).collect();
            types.insert((*n).into(), serde_json::Value::Array(fs));
        }
        Ok(json!({
            "types": types,
            "primaryType": self.name,
            "domain": domain.to_json(),
            "message": self.json(self.name, message)?,
        }))
    }

    fn json(&self, name: &str, v: &Value) -> Result<serde_json::Value> {
        let fields = self
            .types
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, f)| *f)
            .ok_or_else(|| Error::Signing(format!("{name}: type not defined")))?;
        let Value::Struct(values) = v else {
            return Err(Error::Signing(format!("{name}: expected a struct value")));
        };
        if values.len() != fields.len() {
            return Err(Error::Signing(format!(
                "{name}: expected {} fields, got {}",
                fields.len(),
                values.len()
            )));
        }
        let mut m = serde_json::Map::new();
        for ((fname, ftype), value) in fields.iter().zip(values) {
            m.insert((*fname).into(), self.json_value(ftype, value)?);
        }
        Ok(serde_json::Value::Object(m))
    }

    fn json_value(&self, ty: &str, v: &Value) -> Result<serde_json::Value> {
        Ok(match v {
            Value::List(items) => {
                let inner = ty.strip_suffix("[]").unwrap_or(ty);
                serde_json::Value::Array(items.iter().map(|i| self.json_value(inner, i)).collect::<Result<_>>()?)
            }
            Value::Struct(_) => self.json(ty, v)?,
            Value::Int(n) => json!(n.to_string()),
            Value::U256(w) => json!(u256_decimal(w)),
            Value::Bool(b) => json!(b),
            Value::Address(a) => json!(checksum_address(a)),
            Value::String(s) => json!(s),
            Value::Bytes(b) => json!(hex0x(b)),
        })
    }
}

fn base_type(t: &str) -> &str {
    t.split('[').next().unwrap_or(t)
}

/// encodeType: the primary type, then referenced struct types sorted by name.
fn encode_type(primary: &str, types: &[TypeDef]) -> String {
    fn deps(name: &str, types: &[TypeDef], seen: &mut BTreeSet<String>) {
        if !seen.insert(name.to_owned()) {
            return;
        }
        if let Some((_, fields)) = types.iter().find(|(n, _)| *n == name) {
            for (_, t) in fields.iter() {
                let b = base_type(t);
                if types.iter().any(|(n, _)| *n == b) {
                    deps(b, types, seen);
                }
            }
        }
    }
    let mut seen = BTreeSet::new();
    deps(primary, types, &mut seen);
    seen.remove(primary);
    let one = |n: &str| -> String {
        let fields = types.iter().find(|(t, _)| *t == n).map(|(_, f)| *f).unwrap_or(&[]);
        let parts: Vec<String> = fields.iter().map(|(f, t)| format!("{t} {f}")).collect();
        format!("{n}({})", parts.join(","))
    };
    std::iter::once(primary.to_owned())
        .chain(seen)
        .map(|n| one(&n))
        .collect()
}

pub(crate) fn u128_word(n: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&n.to_be_bytes());
    w
}

pub fn hex0x(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("0x");
    for x in b {
        s.push(HEX[(x >> 4) as usize] as char);
        s.push(HEX[(x & 15) as usize] as char);
    }
    s
}

/// EIP-55 mixed-case address, as eth_signTypedData tooling prints it.
pub fn checksum_address(a: &[u8; 20]) -> String {
    let lower = hex0x(a);
    let hash = keccak256(&lower.as_bytes()[2..]);
    let mut out = String::with_capacity(42);
    out.push_str("0x");
    for (i, c) in lower[2..].chars().enumerate() {
        let nibble = (hash[i / 2] >> if i % 2 == 0 { 4 } else { 0 }) & 0x0f;
        out.push(if nibble >= 8 { c.to_ascii_uppercase() } else { c });
    }
    out
}

/// Big-endian 256-bit integer as a decimal string.
fn u256_decimal(w: &[u8; 32]) -> String {
    let mut digits = Vec::new();
    let mut n = *w;
    while n.iter().any(|b| *b != 0) {
        let mut rem = 0u32;
        for b in n.iter_mut() {
            let cur = (rem << 8) | *b as u32;
            *b = (cur / 10) as u8;
            rem = cur % 10;
        }
        digits.push(b'0' + rem as u8);
    }
    if digits.is_empty() {
        return "0".into();
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_hash_and_decimal() {
        static T: &[TypeDef] = &[("T", &[("a", "uint8"), ("b", "int8")])];
        let th = hex0x(&keccak256(b"T(uint8 a,int8 b)"));
        assert_eq!(
            StructType::new("T", T, &th).unwrap().type_hash(),
            keccak256(b"T(uint8 a,int8 b)")
        );
        assert!(StructType::new("T", T, "0x00").is_err());
        assert_eq!(u256_decimal(&u128_word(197889)), "197889");
    }
}
