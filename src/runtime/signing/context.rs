//! Signing without a client: the context a generated sign function runs in, what a signer is asked to
//! sign, and the two-step split (digests, then attach) for signatures made elsewhere.
//!
//! Per signed call the generator emits `sign_<call>(&mut req, &ctx)`, `needs_<call>(&req)` and, for calls
//! that sign every element of a list (bulk orders), `digests_<call>` / `attach_<call>`. All of them are
//! synchronous and read metadata only from `SignContext::metadata`; nothing here does I/O.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value as Json;

use super::eip712::{grvt_domain, StructType, Value};
use super::signer::{check_signature_shape, verify_signature, Signature};
use super::wire::Meta;
use crate::runtime::clock::{NonceProvider, RandomNonce};
use crate::runtime::error::{Error, Result};
use crate::Timestamp;

/// A signer the core sign functions call directly: a local key, or a KMS/HSM/wallet bridge that can
/// answer synchronously. `PrivateKeySigner` implements it.
///
/// The signer only sees the digest the SDK computed (and, on demand, its typed data), so it cannot sign
/// a different message. The SDK checks every returned signature's shape (32-byte r and s, v 27 or 28,
/// low-s, signer field empty or `address()`); whether it came from the right key is checked only when
/// `SignContext::recover_signer` is set.
pub trait SyncSigner {
    /// 0x-prefixed lowercase address.
    fn address(&self) -> &str;
    fn sign(&self, request: &SigningRequest<'_>) -> Result<Signature>;
}

/// What a signer is asked to sign. Only `digest` is signed; the rest says what it is, for signers that
/// log or check requests.
#[derive(Clone)]
pub struct SigningRequest<'a> {
    /// keccak256(0x1901 || domain_separator || struct_hash)
    pub digest: [u8; 32],
    pub struct_hash: [u8; 32],
    pub domain_separator: [u8; 32],
    /// The chain id of the EIP-712 domain: the signature object's own chain id, else the context's.
    pub chain_id: u64,
    pub struct_type: &'static StructType,
    typed: Option<&'a (dyn Fn() -> Result<Json> + Send + Sync)>,
}

impl std::fmt::Debug for SigningRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningRequest")
            .field("primary_type", &self.struct_type.name)
            .field("digest", &super::hex0x(&self.digest))
            .field("chain_id", &self.chain_id)
            .finish_non_exhaustive()
    }
}

impl SigningRequest<'_> {
    /// The EIP-712 primary type, e.g. "Order".
    pub fn primary_type(&self) -> &'static str {
        self.struct_type.name
    }

    /// The eth_signTypedData_v4 JSON of what is signed (types, primaryType, domain, message), for signers
    /// that sign typed data rather than a digest. Built only when called, and only on the request a sign
    /// function hands to `SyncSigner::sign`; requests returned by `digests_*` have none (`Error::Config`).
    pub fn typed_data(&self) -> Result<Json> {
        match self.typed {
            Some(build) => build(),
            None => Err(Error::Config(
                "typed data is only available on the request a sign function passes to SyncSigner::sign".into(),
            )),
        }
    }

    pub(crate) fn new(
        chain_id: u64,
        struct_type: &'static StructType,
        struct_hash: [u8; 32],
    ) -> SigningRequest<'static> {
        let domain_separator = super::wire::domain_separator(chain_id);
        SigningRequest {
            digest: super::wire::digest_with(&domain_separator, &struct_hash),
            struct_hash,
            domain_separator,
            chain_id,
            struct_type,
            typed: None,
        }
    }
}

/// The instruments, currencies and main account one sign call reads from `Meta` (generated
/// `needs_<call>`). Load them once (get_instrument, get_currency, funding_account_summary) and keep them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SigningNeeds {
    pub instruments: Vec<String>,
    pub currencies: Vec<String>,
    pub main_account: bool,
}

/// Everything a sign function needs besides the request.
pub struct SignContext<'a> {
    pub signer: &'a dyn SyncSigner,
    /// The metadata `needs_<call>` names, already loaded.
    pub metadata: &'a Meta,
    /// The environment's chain id; a signature object's own chain id wins when set.
    pub chain_id: u64,
    /// For the default expiration; `None` reads the system clock.
    pub now: Option<Timestamp>,
    /// Signature nonces for signature objects without one; `None` reads the OS random source.
    pub nonces: Option<&'a dyn NonceProvider>,
    /// Check orders against their instrument limits before signing (default true).
    pub check_orders: bool,
    /// Also recover each signature's address and compare it with `signer.address()`: catches a remote
    /// signer using the wrong key, at the cost of one ecrecover (about as slow as signing) per signature.
    pub recover_signer: bool,
}

impl std::fmt::Debug for SignContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignContext")
            .field("signer", &self.signer.address())
            .field("chain_id", &self.chain_id)
            .field("now", &self.now)
            .field("check_orders", &self.check_orders)
            .field("recover_signer", &self.recover_signer)
            .finish_non_exhaustive()
    }
}

impl<'a> SignContext<'a> {
    /// Order checks on, signer recovery off, system clock, OS random nonces.
    pub fn new(signer: &'a dyn SyncSigner, metadata: &'a Meta, chain_id: u64) -> SignContext<'a> {
        SignContext {
            signer,
            metadata,
            chain_id,
            now: None,
            nonces: None,
            check_orders: true,
            recover_signer: false,
        }
    }

    /// What preparing a request reads from this context.
    pub fn prepare_context(&self) -> PrepareContext<'a> {
        PrepareContext {
            address: self.signer.address(),
            metadata: self.metadata,
            chain_id: self.chain_id,
            now: self.now,
            nonces: self.nonces,
            check_orders: self.check_orders,
        }
    }
}

/// What filling, checking and hashing a request reads: the signer's address (not the signer), metadata,
/// chain id, clock, nonce source and the order-check switch. `SignContext::prepare_context` builds it; the
/// client builds it from its own signer.
#[derive(Clone, Copy)]
pub struct PrepareContext<'a> {
    pub address: &'a str,
    pub metadata: &'a Meta,
    pub chain_id: u64,
    pub now: Option<Timestamp>,
    pub nonces: Option<&'a dyn NonceProvider>,
    pub check_orders: bool,
}

impl std::fmt::Debug for PrepareContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrepareContext")
            .field("address", &self.address)
            .field("chain_id", &self.chain_id)
            .field("now", &self.now)
            .field("check_orders", &self.check_orders)
            .finish_non_exhaustive()
    }
}

impl PrepareContext<'_> {
    pub(crate) fn now_ns(&self) -> i64 {
        match self.now {
            Some(t) => t.nanoseconds(),
            None => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0),
        }
    }

    pub(crate) fn next_nonce(&self) -> Result<u32> {
        match self.nonces {
            Some(n) => n.try_next_nonce(),
            None => RandomNonce.try_next_nonce(),
        }
    }
}

/// How one signed call is signed: the generated pieces (`signing::SIGNING_<CALL>`) that `sign`, `digests`
/// and `attach` drive. The client drives them too, with its async `Signer`.
#[doc(hidden)]
pub struct SignSpec<Req> {
    /// e.g. "order.create_order"
    pub call: &'static str,
    pub needs: fn(&Req) -> SigningNeeds,
    /// How many signatures the request takes (0 for a list left out).
    pub count: fn(&Req) -> usize,
    /// Fill each signature object's signer, nonce, expiration and chain id when absent, check orders
    /// (when the bool is set and the context asks), and hash: one request per signature, in order. On an
    /// error every signature object is put back as it was.
    pub prepare: fn(&mut Req, &PrepareContext<'_>, bool) -> Result<Vec<SigningRequest<'static>>>,
    /// Every signature object has its signer, nonce, expiration and chain id.
    pub prepared: fn(&Req) -> bool,
    /// The EIP-712 message of signature `i`, for typed-data signers.
    pub message: fn(&Req, &Meta, usize) -> Result<Value>,
    /// Write the signatures in, in the order `prepare` returned them.
    pub commit: fn(&mut Req, Vec<Signature>),
}

impl<Req> std::fmt::Debug for SignSpec<Req> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SignSpec({})", self.call)
    }
}

/// The body of every generated `sign_<call>`: fill each signature object's signer, nonce, expiration and
/// chain id when absent, check orders, hash, sign each digest with `ctx.signer`, check each signature,
/// and write r/s/v. A failed check or hash changes nothing. If the signer fails or returns a bad
/// signature, no r/s/v is written but the filled signer, nonce, expiration and chain id stay (signing
/// again reuses them).
pub fn sign<Req: Sync>(req: &mut Req, ctx: &SignContext<'_>, spec: &SignSpec<Req>) -> Result<()> {
    let pending = (spec.prepare)(req, &ctx.prepare_context(), true)?;
    let mut signatures = Vec::with_capacity(pending.len());
    {
        let signed: &Req = req;
        let (address, metadata, message) = (ctx.signer.address(), ctx.metadata, spec.message);
        for (i, p) in pending.iter().enumerate() {
            let (struct_type, chain_id) = (p.struct_type, p.chain_id);
            let typed = move || -> Result<Json> {
                let message = message(signed, metadata, i)?;
                struct_type.to_typed_data(&grvt_domain(chain_id), &message)
            };
            let request = SigningRequest {
                typed: Some(&typed),
                ..p.clone()
            };
            let signature = ctx.signer.sign(&request)?;
            if ctx.recover_signer {
                verify_signature(&p.digest, &signature, address)?;
            } else {
                check_signature_shape(&signature, address)?;
            }
            signatures.push(signature);
        }
    }
    (spec.commit)(req, signatures);
    Ok(())
}

/// The body of every generated `digests_<call>`: fill every element's signer, nonce, expiration and chain
/// id when absent, check and hash every element, and return one request per element, in order. If any
/// element fails, no element is changed. Change nothing in the request until `attach`.
pub fn digests<Req>(
    req: &mut Req,
    ctx: &SignContext<'_>,
    spec: &SignSpec<Req>,
) -> Result<Vec<SigningRequest<'static>>> {
    (spec.prepare)(req, &ctx.prepare_context(), true)
}

/// The body of every generated `attach_<call>`: one signature per element, in the order `digests`
/// returned them. Each one must recover to `ctx.signer.address()` against its element's digest (always,
/// whatever `recover_signer` says), so a signature attached to the wrong element is refused. Nothing is
/// written unless every signature passes.
pub fn attach<Req>(req: &mut Req, ctx: &SignContext<'_>, signatures: &[Signature], spec: &SignSpec<Req>) -> Result<()> {
    let n = (spec.count)(req);
    if signatures.len() != n {
        return Err(Error::Config(format!(
            "{} signatures for {n} elements of {}",
            signatures.len(),
            spec.call
        )));
    }
    if !(spec.prepared)(req) {
        return Err(Error::Config(format!(
            "{}: a signature object is not prepared; call the digests function first",
            spec.call
        )));
    }
    let pending = (spec.prepare)(req, &ctx.prepare_context(), false)?;
    let address = ctx.signer.address();
    for (p, s) in pending.iter().zip(signatures) {
        verify_signature(&p.digest, s, address)?;
    }
    (spec.commit)(req, signatures.to_vec());
    Ok(())
}
