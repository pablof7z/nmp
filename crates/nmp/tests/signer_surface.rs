//! Compile proof that a consumer depending only on `nmp` can implement the
//! promoted signing capability without importing mechanism crates.

use nmp::{Event, PublicKey, SignerError, SignerOp, SigningCapability, UnsignedEvent};

struct ConsumerSigner;

impl SigningCapability for ConsumerSigner {
    fn public_key(&self) -> Option<PublicKey> {
        None
    }

    fn sign(&self, _unsigned: UnsignedEvent) -> SignerOp<Event> {
        SignerOp::err(SignerError::Unavailable)
    }
}

#[test]
fn nmp_only_signer_surface_is_implementable() {
    let signer = ConsumerSigner;
    assert_eq!(signer.public_key(), None);
}
