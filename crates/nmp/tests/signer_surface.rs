//! Compile-and-operation proof that a consumer depending only on `nmp` can
//! implement the signer/crypto interface without importing a provider crate.

use nmp::{
    CryptoCapability, SignerOp, SignerPublicKey, SignerSignedEvent, SignerUnsignedEvent,
    SigningCapability,
};

struct ConsumerSigner;

impl SigningCapability for ConsumerSigner {
    fn public_key(&self) -> Option<SignerPublicKey> {
        Some(SignerPublicKey::new([2; 32]))
    }

    fn sign(&self, unsigned: SignerUnsignedEvent) -> SignerOp<SignerSignedEvent> {
        let (public_key, created_at, kind, tags, content) = unsigned.into_parts();
        SignerOp::ok(SignerSignedEvent::new(
            [0; 32], public_key, created_at, kind, tags, content, [0; 64],
        ))
    }
}

impl CryptoCapability for ConsumerSigner {
    fn nip44_encrypt(&self, _peer: SignerPublicKey, plaintext: &str) -> SignerOp<String> {
        SignerOp::ok(format!("opaque:{plaintext}"))
    }

    fn nip44_decrypt(&self, _peer: SignerPublicKey, ciphertext: &str) -> SignerOp<String> {
        SignerOp::ok(
            ciphertext
                .strip_prefix("opaque:")
                .unwrap_or(ciphertext)
                .to_string(),
        )
    }
}

#[test]
fn nmp_only_signer_surface_is_implementable() {
    let signer = ConsumerSigner;
    let public_key = signer.public_key().unwrap();
    let signed = signer
        .sign(SignerUnsignedEvent::new(
            public_key,
            1,
            1,
            Vec::new(),
            "hello".to_string(),
        ))
        .wait(std::time::Duration::ZERO)
        .unwrap();
    assert_eq!(signed.public_key(), public_key);
    assert_eq!(signed.content(), "hello");

    let encrypted = signer
        .nip44_encrypt(public_key, "secret")
        .wait(std::time::Duration::ZERO)
        .unwrap();
    assert_eq!(
        signer
            .nip44_decrypt(public_key, &encrypted)
            .wait(std::time::Duration::ZERO)
            .unwrap(),
        "secret"
    );
}
