//! EIP-712 signing: the typed-data encoder, the signer plug-ins, the sign context, and the wire
//! transforms and field writers the generated sign and hash functions use. No I/O.

pub mod checks;
pub mod context;
pub mod eip712;
pub mod signer;
pub mod wire;

pub use checks::{check_order, OrderCheckRule, OrderChecks, Scale};
pub use context::{
    attach, digests, sign, PrepareContext, SignContext, SignSpec, SigningNeeds, SigningRequest, SyncSigner,
};
pub use eip712::{grvt_domain, hex0x, Domain, StructType, TypeDef, Value};
#[cfg(feature = "client")]
pub use signer::Signer;
pub use signer::{check_signature_shape, recover_address, verify_signature, PrivateKeySigner, Signature};
pub use wire::{digest, domain_separator, CurrencyMeta, InstrumentMeta, Meta};

/// keccak256 (`sha3` crate; measured slightly faster than `tiny-keccak` in benches/hot_path.rs).
#[inline]
pub fn keccak256(data: &[u8]) -> [u8; 32] {
    use sha3::{Digest, Keccak256};
    Keccak256::digest(data).into()
}
