//! Compile-and-operation proof that a consumer depending only on `nmp` can
//! implement the signer/crypto interface without importing a provider crate.

use nmp::{
    DecryptCapability, DecryptPayloadRequest, EncryptCapability, EncryptPayloadRequest,
    EncryptedPayload, EncryptedPayloadService, PayloadEncryption, PayloadFence, PayloadLimits,
    SignerOp, SignerPublicKey, SignerSignedEvent, SignerUnsignedEvent, SigningCapability,
    TransientPlaintext,
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

impl EncryptCapability for ConsumerSigner {
    fn encrypt(&self, request: EncryptPayloadRequest) -> SignerOp<EncryptedPayload> {
        let (_, _, plaintext) = request.into_parts();
        SignerOp::ok(EncryptedPayload::new(format!(
            "opaque:{}",
            plaintext.as_str().unwrap()
        )))
    }
}

impl DecryptCapability for ConsumerSigner {
    fn decrypt(&self, request: DecryptPayloadRequest) -> SignerOp<TransientPlaintext> {
        let (_, _, ciphertext) = request.into_parts();
        let plaintext = ciphertext.strip_prefix("opaque:").unwrap_or(&ciphertext);
        SignerOp::ok(TransientPlaintext::new(plaintext.as_bytes().to_vec()))
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

    let fence = PayloadFence::new([1; 32], [2; 32], 3, [4; 32]);
    let limits = PayloadLimits::new(64, 64);
    let encrypted = EncryptedPayloadService::encrypt(
        &signer,
        fence,
        PayloadEncryption::Nip44V2,
        public_key,
        TransientPlaintext::new(b"secret".to_vec()),
        limits,
    )
    .unwrap()
    .wait(std::time::Duration::ZERO)
    .unwrap()
    .accept(fence)
    .unwrap();
    let decrypted = EncryptedPayloadService::decrypt(
        &signer,
        fence,
        PayloadEncryption::Nip44V2,
        public_key,
        encrypted.into_string(),
        limits,
    )
    .unwrap()
    .wait(std::time::Duration::ZERO)
    .unwrap()
    .accept(fence)
    .unwrap();
    assert_eq!(
        decrypted
            .as_str()
            .expect("consumer capability returned UTF-8"),
        "secret"
    );
}
