use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use super::*;
use crate::{PendingSignerResolveError, SignerOp};

struct ReadyCrypto {
    decrypt_calls: AtomicUsize,
    encrypt_calls: AtomicUsize,
}

impl ReadyCrypto {
    fn new() -> Self {
        Self {
            decrypt_calls: AtomicUsize::new(0),
            encrypt_calls: AtomicUsize::new(0),
        }
    }
}

impl DecryptCapability for ReadyCrypto {
    fn decrypt(&self, request: DecryptPayloadRequest) -> SignerOp<TransientPlaintext> {
        self.decrypt_calls.fetch_add(1, Ordering::SeqCst);
        let (_, _, ciphertext) = request.into_parts();
        SignerOp::ok(TransientPlaintext::new(ciphertext.into_bytes()))
    }
}

impl EncryptCapability for ReadyCrypto {
    fn encrypt(&self, request: EncryptPayloadRequest) -> SignerOp<EncryptedPayload> {
        self.encrypt_calls.fetch_add(1, Ordering::SeqCst);
        let (_, _, plaintext) = request.into_parts();
        SignerOp::ok(EncryptedPayload::new(
            String::from_utf8(plaintext.as_bytes().to_vec()).unwrap(),
        ))
    }
}

fn fence(source: u8, revision: u64) -> PayloadFence {
    PayloadFence::new([source; 32], [9; 32], revision, [7; 32])
}

fn peer() -> SignerPublicKey {
    SignerPublicKey::new([3; 32])
}

#[test]
fn limits_refuse_before_or_immediately_after_capability_work() {
    let crypto = ReadyCrypto::new();
    let limits = PayloadLimits::new(3, 2);

    assert_eq!(
        EncryptedPayloadService::decrypt(
            &crypto,
            fence(1, 1),
            PayloadEncryption::Nip44V2,
            peer(),
            "four".to_string(),
            limits,
        )
        .err(),
        Some(PayloadError::CiphertextTooLarge { actual: 4, max: 3 })
    );
    assert_eq!(crypto.decrypt_calls.load(Ordering::SeqCst), 0);

    let result = EncryptedPayloadService::decrypt(
        &crypto,
        fence(1, 1),
        PayloadEncryption::Nip44V2,
        peer(),
        "abc".to_string(),
        limits,
    )
    .unwrap()
    .wait(Duration::from_millis(1));
    assert!(matches!(
        result,
        Err(PayloadError::PlaintextTooLarge { actual: 3, max: 2 })
    ));

    assert!(matches!(
        EncryptedPayloadService::encrypt(
            &crypto,
            fence(1, 1),
            PayloadEncryption::Nip44V2,
            peer(),
            TransientPlaintext::new(b"abc".to_vec()),
            limits,
        ),
        Err(PayloadError::PlaintextTooLarge { actual: 3, max: 2 })
    ));
    assert_eq!(crypto.encrypt_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn source_and_target_fence_reject_stale_results() {
    let crypto = ReadyCrypto::new();
    let limits = PayloadLimits::new(32, 32);
    let result = EncryptedPayloadService::decrypt(
        &crypto,
        fence(1, 4),
        PayloadEncryption::Nip04,
        peer(),
        "secret".to_string(),
        limits,
    )
    .unwrap()
    .wait(Duration::from_millis(1))
    .unwrap();

    assert!(matches!(
        result.accept(fence(2, 4)),
        Err(StalePayloadResult)
    ));

    let result = EncryptedPayloadService::encrypt(
        &crypto,
        fence(1, 4),
        PayloadEncryption::Nip04,
        peer(),
        TransientPlaintext::new(b"secret".to_vec()),
        limits,
    )
    .unwrap()
    .wait(Duration::from_millis(1))
    .unwrap();
    assert!(matches!(
        result.accept(fence(1, 5)),
        Err(StalePayloadResult)
    ));
}

struct PendingDecrypt {
    sender: std::sync::Mutex<Option<crate::PendingSignerSender<TransientPlaintext>>>,
    cancelled: Arc<AtomicBool>,
}

impl DecryptCapability for PendingDecrypt {
    fn decrypt(&self, _request: DecryptPayloadRequest) -> SignerOp<TransientPlaintext> {
        let cancelled = Arc::clone(&self.cancelled);
        let (sender, operation) = SignerOp::pending_channel_with_cancel(move || {
            cancelled.store(true, Ordering::SeqCst);
        });
        *self.sender.lock().unwrap() = Some(sender);
        operation
    }
}

#[test]
fn dropped_operation_cancels_and_late_plaintext_is_refused() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let crypto = PendingDecrypt {
        sender: std::sync::Mutex::new(None),
        cancelled: Arc::clone(&cancelled),
    };
    let operation = EncryptedPayloadService::decrypt(
        &crypto,
        fence(1, 1),
        PayloadEncryption::Nip44V2,
        peer(),
        "ciphertext".to_string(),
        PayloadLimits::new(32, 32),
    )
    .unwrap();
    let sender = crypto.sender.lock().unwrap().take().unwrap();

    drop(operation);
    assert!(cancelled.load(Ordering::SeqCst));
    assert!(matches!(
        sender.resolve(Ok(TransientPlaintext::new(b"late".to_vec()))),
        Err(PendingSignerResolveError::ReceiverDropped(Ok(_)))
    ));
}

#[test]
fn capability_traits_are_independent() {
    struct DecryptOnly;
    impl DecryptCapability for DecryptOnly {
        fn decrypt(&self, _: DecryptPayloadRequest) -> SignerOp<TransientPlaintext> {
            SignerOp::ok(TransientPlaintext::new(Vec::new()))
        }
    }

    let capability: &dyn DecryptCapability = &DecryptOnly;
    let result = EncryptedPayloadService::decrypt(
        capability,
        fence(1, 1),
        PayloadEncryption::Nip44V2,
        peer(),
        String::new(),
        PayloadLimits::new(1, 1),
    )
    .unwrap()
    .wait(Duration::from_millis(1))
    .unwrap()
    .accept(fence(1, 1))
    .unwrap();
    assert!(result.is_empty());
}
