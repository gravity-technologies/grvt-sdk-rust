//! The generic EIP-712 encoder, kept only as the test reference for the generated encoders: it
//! interprets a struct type's definitions at runtime. Shared by tests/encoders.rs, tests/signing_cases.rs and
//! benches/hot_path.rs (`#[path]`).

#![allow(dead_code)]

use grvt::runtime::signing::keccak256;
use grvt::{Domain, Error, Result, StructType, Value};

#[derive(Debug, Clone)]
enum Enc {
    Uint(u32),
    Int(u32),
    Bool,
    Address,
    FixedBytes(usize),
    String,
    Bytes,
    Struct(&'static str),
    Array(Box<Enc>),
}

fn encoder(st: &StructType, t: &str) -> Result<Enc> {
    if let Some(inner) = t.strip_suffix(']') {
        let open = inner
            .rfind('[')
            .ok_or_else(|| Error::Signing(format!("bad type {t}")))?;
        return Ok(Enc::Array(Box::new(encoder(st, &inner[..open])?)));
    }
    if let Some((n, _)) = st.types.iter().find(|(n, _)| *n == t) {
        return Ok(Enc::Struct(n));
    }
    let bits = |s: &str| -> Result<u32> {
        if s.is_empty() {
            Ok(256)
        } else {
            s.parse().map_err(|_| Error::Signing(format!("bad type {t}")))
        }
    };
    Ok(match t {
        "bool" => Enc::Bool,
        "address" => Enc::Address,
        "string" => Enc::String,
        "bytes" => Enc::Bytes,
        _ if t.starts_with("uint") => Enc::Uint(bits(&t[4..])?),
        _ if t.starts_with("int") => Enc::Int(bits(&t[3..])?),
        _ if t.starts_with("bytes") => Enc::FixedBytes(bits(&t[5..])? as usize),
        _ => return Err(Error::Signing(format!("unsupported EIP-712 type {t}"))),
    })
}

/// encodeType of `name` within the struct type's definitions.
fn encode_type(st: &StructType, name: &str) -> String {
    fn deps(st: &StructType, name: &str, seen: &mut std::collections::BTreeSet<String>) {
        if !seen.insert(name.to_owned()) {
            return;
        }
        if let Some((_, fields)) = st.types.iter().find(|(n, _)| *n == name) {
            for (_, t) in fields.iter() {
                let b = t.split('[').next().unwrap_or(t);
                if st.types.iter().any(|(n, _)| *n == b) {
                    deps(st, b, seen);
                }
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    deps(st, name, &mut seen);
    seen.remove(name);
    let one = |n: &str| {
        let fields = st.types.iter().find(|(t, _)| *t == n).map(|(_, f)| *f).unwrap_or(&[]);
        let parts: Vec<String> = fields.iter().map(|(f, t)| format!("{t} {f}")).collect();
        format!("{n}({})", parts.join(","))
    };
    std::iter::once(name.to_owned()).chain(seen).map(|n| one(&n)).collect()
}

fn word(n: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&n.to_be_bytes());
    w
}

fn hash(st: &StructType, name: &str, v: &Value) -> Result<[u8; 32]> {
    let fields = st.types.iter().find(|(n, _)| *n == name).map(|(_, f)| *f).unwrap();
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
    let mut buf = Vec::with_capacity(32 * (1 + values.len()));
    buf.extend_from_slice(&keccak256(encode_type(st, name).as_bytes()));
    for ((fname, ftype), value) in fields.iter().zip(values) {
        let enc = encoder(st, ftype)?;
        buf.extend_from_slice(&encode(st, &enc, value).map_err(|e| prefix(e, name, fname))?);
    }
    Ok(keccak256(&buf))
}

fn encode(st: &StructType, enc: &Enc, v: &Value) -> Result<[u8; 32]> {
    match (enc, v) {
        (Enc::Uint(bits), Value::Int(n)) => {
            if *n < 0 || (*bits < 128 && *n >= 1i128 << bits) {
                return Err(Error::Signing(format!("value {n} out of range for uint{bits}")));
            }
            Ok(word(*n as u128))
        }
        (Enc::Uint(bits), Value::U256(w)) => {
            let used = 256 - w.iter().take_while(|b| **b == 0).count() as u32 * 8;
            if used > *bits {
                return Err(Error::Signing(format!(
                    "value {} out of range for uint{bits}",
                    grvt::runtime::hex0x(w)
                )));
            }
            Ok(*w)
        }
        (Enc::Int(bits), Value::Int(n)) => {
            let lim = if *bits >= 128 { i128::MAX } else { 1i128 << (bits - 1) };
            if *bits < 128 && (*n < -lim || *n >= lim) {
                return Err(Error::Signing(format!("value {n} out of range for int{bits}")));
            }
            let mut w = if *n < 0 { [0xffu8; 32] } else { [0u8; 32] };
            w[16..].copy_from_slice(&n.to_be_bytes());
            Ok(w)
        }
        (Enc::Bool, Value::Bool(b)) => Ok(word(*b as u128)),
        (Enc::Address, Value::Address(a)) => {
            let mut w = [0u8; 32];
            w[12..].copy_from_slice(a);
            Ok(w)
        }
        (Enc::FixedBytes(n), Value::Bytes(b)) if b.len() == *n => {
            let mut w = [0u8; 32];
            w[..*n].copy_from_slice(b);
            Ok(w)
        }
        (Enc::String, Value::String(s)) => Ok(keccak256(s.as_bytes())),
        (Enc::Bytes, Value::Bytes(b)) => Ok(keccak256(b)),
        (Enc::Struct(n), v) => hash(st, n, v),
        (Enc::Array(inner), Value::List(items)) => {
            let mut buf = Vec::with_capacity(32 * items.len());
            for item in items {
                buf.extend_from_slice(&encode(st, inner, item)?);
            }
            Ok(keccak256(&buf))
        }
        (enc, v) => Err(Error::Signing(format!("value {v:?} does not fit {enc:?}"))),
    }
}

fn prefix(e: Error, struct_name: &str, field: &str) -> Error {
    match e {
        Error::Signing(m) if !m.contains(": ") || m.starts_with("value") => {
            Error::Signing(format!("{struct_name}.{field}: {m}"))
        }
        other => other,
    }
}

/// hashStruct(message) for the primary type.
pub fn hash_struct(st: &StructType, message: &Value) -> Result<[u8; 32]> {
    hash(st, st.name, message)
}

/// keccak256(0x1901 || domainSeparator || hashStruct(message)).
pub fn digest(st: &StructType, domain: &Domain, message: &Value) -> Result<[u8; 32]> {
    let mut buf = [0u8; 66];
    buf[0] = 0x19;
    buf[1] = 0x01;
    buf[2..34].copy_from_slice(&domain.separator());
    buf[34..].copy_from_slice(&hash_struct(st, message)?);
    Ok(keccak256(&buf))
}
