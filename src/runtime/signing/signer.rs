//! Signers.
//!
//! - `SyncSigner` (core, in `context`): the sign functions call it with a `SigningRequest` and get r/s/v.
//! - `Signer` (client, async): the client calls `sign_prehash` with the digest first; a signer without an
//!   in-process key returns `None` and gets the typed message through `sign_typed` instead.
//!
//! `PrivateKeySigner` implements both.

use std::sync::LazyLock;

use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use secp256k1::{All, Message, PublicKey, Secp256k1, SecretKey, VerifyOnly};

use super::context::{SigningRequest, SyncSigner};
use super::eip712::hex0x;
#[cfg(feature = "client")]
use super::eip712::{Domain, StructType, Value};
use super::keccak256;
use crate::runtime::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// 0x-prefixed lowercase address.
    pub signer: String,
    /// 0x-prefixed 32-byte hex.
    pub r: String,
    /// 0x-prefixed 32-byte hex, low-s (the server rejects high s, code 2007).
    pub s: String,
    /// 27 or 28.
    pub v: u8,
}

/// The client's signer plug-in. A signer gets the exact EIP-712 input (domain, primary type with its
/// definitions, message) and returns r/s/v. It needs no Grvt knowledge: a KMS/HSM/wallet signer can call
/// `primary_type.to_typed_data(domain, message)` and hand that JSON to its backend.
#[cfg(feature = "client")]
pub trait Signer: Send + Sync {
    /// 0x-prefixed lowercase address.
    fn address(&self) -> &str;

    fn sign_typed<'a>(
        &'a self,
        domain: &'a Domain,
        primary_type: &'a StructType,
        message: &'a Value,
    ) -> futures::future::BoxFuture<'a, Result<Signature>>;

    /// Sign a ready EIP-712 digest. The SDK calls this first: a signer that holds its key in
    /// process returns `Some`, and the SDK never builds the typed message. A wallet/KMS signer
    /// returns `None`, and the SDK calls `sign_typed` instead. A signer that wraps another one
    /// must forward this call too: `PrivateKeySigner` signs only through it.
    fn sign_prehash(&self, digest: &[u8; 32]) -> Option<Result<Signature>>;
}

/// In-process secp256k1 key (libsecp256k1 via the `secp256k1` crate). Also fits session keys. Safe to
/// share between threads (e.g. to sign the digests of `digests_order_bulk_orders` in parallel).
#[derive(Clone)]
pub struct PrivateKeySigner {
    key: SecretKey,
    secp: Secp256k1<All>,
    address: String,
}

impl std::fmt::Debug for PrivateKeySigner {
    // never print the key
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PrivateKeySigner({})", self.address)
    }
}

impl PrivateKeySigner {
    /// `private_key`: 32 bytes as hex, with or without 0x. Fails if the OS random source does not answer
    /// (it blinds the key's signing context).
    ///
    /// Erasing the key is best effort: the signer wipes its own copy on drop, but `SecretKey` is `Copy`, so
    /// the compiler can leave copies on the stack while the signer is built, and every clone holds one.
    pub fn new(private_key: &str) -> Result<PrivateKeySigner> {
        // Never put the input in an error: it is the key, or close to it.
        let mut raw = hex32(private_key)
            .ok_or_else(|| Error::Config("bad private key: it must be 32 bytes of hex, with or without 0x".into()))?;
        let mut seed = [0u8; 32];
        let seeded = rand::TryRng::try_fill_bytes(&mut rand::rngs::SysRng, &mut seed)
            .map(|()| seed)
            .map_err(|e| e.to_string());
        let signer = Self::with_seed(&raw, seeded);
        wipe(&mut raw);
        signer
    }

    /// One context per key, blinded once with OS randomness (side-channel hardening at no cost per
    /// signature). The key is built here, once, so no `SecretKey` exists when the seed is missing.
    fn with_seed(raw: &[u8; 32], seed: std::result::Result<[u8; 32], String>) -> Result<PrivateKeySigner> {
        let mut seed = seed.map_err(|e| Error::Config(format!("the OS random source failed: {e}")))?;
        let key = SecretKey::from_byte_array(*raw).map_err(|e| Error::Config(format!("bad private key: {e}")));
        let mut secp = Secp256k1::new();
        secp.seeded_randomize(&seed);
        wipe(&mut seed);
        let key = key?;
        let pubkey = PublicKey::from_secret_key(&secp, &key).serialize_uncompressed();
        let address = hex0x(&keccak256(&pubkey[1..])[12..]);
        Ok(PrivateKeySigner { key, secp, address })
    }

    /// 0x-prefixed lowercase address.
    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn sign_digest(&self, digest: &[u8; 32]) -> Signature {
        // libsecp256k1 always returns low-s.
        let sig = self
            .secp
            .sign_ecdsa_recoverable(Message::from_digest(*digest), &self.key);
        let (rec, bytes) = sig.serialize_compact();
        Signature {
            signer: self.address.clone(),
            r: hex0x(&bytes[..32]),
            s: hex0x(&bytes[32..]),
            v: 27 + i32::from(rec) as u8,
        }
    }
}

impl Drop for PrivateKeySigner {
    // Clones hold their own copy and wipe it too.
    fn drop(&mut self) {
        self.key.non_secure_erase();
    }
}

/// Overwrite secret bytes so they do not linger in freed memory.
fn wipe(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

impl SyncSigner for PrivateKeySigner {
    fn address(&self) -> &str {
        &self.address
    }

    fn sign(&self, request: &SigningRequest<'_>) -> Result<Signature> {
        Ok(self.sign_digest(&request.digest))
    }
}

#[cfg(feature = "client")]
impl Signer for PrivateKeySigner {
    fn address(&self) -> &str {
        &self.address
    }

    fn sign_typed<'a>(
        &'a self,
        domain: &'a Domain,
        primary_type: &'a StructType,
        message: &'a Value,
    ) -> futures::future::BoxFuture<'a, Result<Signature>> {
        // The SDK signs with this key through sign_prehash; there is no generic hashing to fall back on.
        let _ = (domain, message);
        Box::pin(std::future::ready(Err(Error::Signing(format!(
            "PrivateKeySigner signs digests (sign_prehash), not typed {} messages",
            primary_type.name
        )))))
    }

    fn sign_prehash(&self, digest: &[u8; 32]) -> Option<Result<Signature>> {
        Some(Ok(self.sign_digest(digest)))
    }
}

/// Half the secp256k1 group order: a valid signature has `s <= HALF_ORDER` (low-s).
const HALF_ORDER: [u8; 32] = [
    0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x5d, 0x57, 0x6e,
    0x73, 0x57, 0xa4, 0x50, 0x1d, 0xdf, 0xe9, 0x2f, 0x46, 0x68, 0x1b, 0x20, 0xa0,
];

fn invalid(address: &str, why: impl std::fmt::Display) -> Error {
    Error::Signing(format!("signer {address} returned an invalid signature: {why}"))
}

/// r, s and v alone: 32-byte r and s, v 27 or 28, low-s.
fn check_rsv(sig: &Signature, address: &str) -> Result<()> {
    let (Some(_), Some(s)) = (hex32(&sig.r), hex32(&sig.s)) else {
        return Err(invalid(address, "r and s must be 32 bytes of hex"));
    };
    if sig.v != 27 && sig.v != 28 {
        return Err(invalid(address, format_args!("v must be 27 or 28, got {}", sig.v)));
    }
    if s > HALF_ORDER {
        return Err(invalid(address, "s is not low-s"));
    }
    Ok(())
}

fn check_signer_field(sig: &Signature, address: &str) -> Result<()> {
    if !sig.signer.is_empty() && !sig.signer.eq_ignore_ascii_case(address) {
        return Err(invalid(
            address,
            format_args!("its signer field {} is not the signer address", sig.signer),
        ));
    }
    Ok(())
}

/// The check the SDK applies to every signature a `SyncSigner` returns, the same rule in every SDK:
/// 32-byte r and s, v 27 or 28, low-s, and `sig.signer` (when set) is `address`. It does not recover
/// the key, so it does not prove the right key signed; see `verify_signature`.
pub fn check_signature_shape(sig: &Signature, address: &str) -> Result<()> {
    check_rsv(sig, address)?;
    check_signer_field(sig, address)
}

/// `check_signature_shape`, plus the signature recovers to `address` against `digest`.
pub fn verify_signature(digest: &[u8; 32], sig: &Signature, address: &str) -> Result<()> {
    check_rsv(sig, address)?;
    match recover_address(digest, sig) {
        Some(got) if got.eq_ignore_ascii_case(address) => {}
        got => {
            let got = got.unwrap_or_else(|| "no address".into());
            return Err(invalid(
                address,
                format_args!(
                    "it recovers to {got}, not to the signer address (wrong key, or a different message was signed)"
                ),
            ));
        }
    }
    check_signer_field(sig, address)
}

/// The 0x lowercase address that signed `digest` with r/s/v (27/28), or `None` if it does not recover.
pub fn recover_address(digest: &[u8; 32], sig: &Signature) -> Option<String> {
    let mut rs = [0u8; 64];
    rs[..32].copy_from_slice(&hex32(&sig.r)?);
    rs[32..].copy_from_slice(&hex32(&sig.s)?);
    let rec = RecoveryId::try_from(i32::from(sig.v.checked_sub(27)?)).ok()?;
    let sig = RecoverableSignature::from_compact(&rs, rec).ok()?;
    static VERIFY: LazyLock<Secp256k1<VerifyOnly>> = LazyLock::new(Secp256k1::verification_only);
    let key = VERIFY.recover_ecdsa(Message::from_digest(*digest), &sig).ok()?;
    Some(hex0x(&keccak256(&key.serialize_uncompressed()[1..])[12..]))
}

/// 32 bytes of hex (with or without 0x), without allocating.
fn hex32(s: &str) -> Option<[u8; 32]> {
    let h = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s)
        .as_bytes();
    if h.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in h.chunks_exact(2).enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_signer_without_os_randomness() {
        let err = PrivateKeySigner::with_seed(&[7; 32], Err("unavailable".into())).unwrap_err();
        assert!(matches!(err, Error::Config(m) if m.contains("OS random source")));
        assert!(PrivateKeySigner::with_seed(&[7; 32], Ok([1; 32])).is_ok());
    }

    #[test]
    fn bad_keys_are_errors_that_do_not_echo_the_key() {
        let hex = "ab".repeat(32);
        for bad in [
            format!("\u{201c}0x{hex}\u{201d}"), // smart quotes: multi-byte chars at odd offsets
            format!("0x{}\u{e9}", &hex[..62]),
            format!("0x{}", &hex[..62]),
            format!("0x{hex}00"),
            format!("0x{}zz", &hex[..62]),
            String::new(),
        ] {
            let err = PrivateKeySigner::new(&bad).unwrap_err();
            assert!(
                matches!(&err, Error::Config(m) if !m.contains(&hex[..16])),
                "{bad:?}: {err}"
            );
        }
        let zero = PrivateKeySigner::new(&"00".repeat(32)).unwrap_err();
        assert!(matches!(zero, Error::Config(m) if m.starts_with("bad private key")));
    }

    #[test]
    fn wipe_zeroes() {
        let mut b = [9u8; 4];
        wipe(&mut b);
        assert_eq!(b, [0; 4]);
    }
}
