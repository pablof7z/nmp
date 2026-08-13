use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::{DecryptCapability, EncryptCapability, PendingSignerResolveError, SignerOp};

struct ReadyCrypto {
    decrypt_calls: AtomicUsize,
    encrypt_calls: AtomicUsize,
    decrypt_scheme: Mutex<Option<PayloadEncryption>>,
    encrypt_scheme: Mutex<Option<PayloadEncryption>>,
}

impl ReadyCrypto {
    fn new() -> Self {
        Self {
            decrypt_calls: AtomicUsize::new(0),
            encrypt_calls: AtomicUsize::new(0),
            decrypt_scheme: Mutex::new(None),
            encrypt_scheme: Mutex::new(None),
        }
    }
}

impl DecryptCapability for ReadyCrypto {
    fn decrypt(&self, request: DecryptPayloadRequest) -> SignerOp<TransientPlaintext> {
        self.decrypt_calls.fetch_add(1, Ordering::SeqCst);
        let (scheme, _, ciphertext) = request.into_parts();
        *self.decrypt_scheme.lock().unwrap() = Some(scheme);
        SignerOp::ok(TransientPlaintext::new(ciphertext.into_bytes()))
    }
}

impl EncryptCapability for ReadyCrypto {
    fn encrypt(&self, request: EncryptPayloadRequest) -> SignerOp<EncryptedPayload> {
        self.encrypt_calls.fetch_add(1, Ordering::SeqCst);
        let (scheme, _, plaintext) = request.into_parts();
        *self.encrypt_scheme.lock().unwrap() = Some(scheme);
        SignerOp::ok(EncryptedPayload::new(
            String::from_utf8(plaintext.as_bytes().to_vec()).unwrap(),
        ))
    }
}

#[test]
fn selected_scheme_is_preserved_for_each_exact_request() {
    let crypto = ReadyCrypto::new();
    let limits = PayloadLimits::new(32, 32);
    EncryptedPayloadService::decrypt(
        &crypto,
        fence_with(1, 1, PayloadEncryption::Nip04, limits),
        "ciphertext".to_string(),
    )
    .unwrap();
    EncryptedPayloadService::encrypt(
        &crypto,
        fence_with(1, 1, PayloadEncryption::Nip44V2, limits),
        TransientPlaintext::new(b"plaintext".to_vec()),
    )
    .unwrap();

    assert_eq!(
        *crypto.decrypt_scheme.lock().unwrap(),
        Some(PayloadEncryption::Nip04)
    );
    assert_eq!(
        *crypto.encrypt_scheme.lock().unwrap(),
        Some(PayloadEncryption::Nip44V2)
    );
}

fn fence(source: u8, revision: u64) -> PayloadFence {
    fence_with(
        source,
        revision,
        PayloadEncryption::Nip44V2,
        PayloadLimits::new(32, 32),
    )
}

fn fence_with(
    source: u8,
    revision: u64,
    scheme: PayloadEncryption,
    limits: PayloadLimits,
) -> PayloadFence {
    PayloadFence::new(
        PayloadSource::Event([source; 32]),
        [9; 32],
        revision,
        [7; 32],
        PayloadPolicy::new([5; 16], 1, PayloadCodecId::new([6; 16]), scheme, peer()),
        limits,
    )
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
            fence_with(1, 1, PayloadEncryption::Nip44V2, limits),
            "four".to_string(),
        )
        .err(),
        Some(PayloadError::CiphertextTooLarge { actual: 4, max: 3 })
    );
    assert_eq!(crypto.decrypt_calls.load(Ordering::SeqCst), 0);

    let result = EncryptedPayloadService::decrypt(
        &crypto,
        fence_with(1, 1, PayloadEncryption::Nip44V2, limits),
        "abc".to_string(),
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
            fence_with(1, 1, PayloadEncryption::Nip44V2, limits),
            TransientPlaintext::new(b"abc".to_vec()),
        ),
        Err(PayloadError::PlaintextTooLarge { actual: 3, max: 2 })
    ));
    assert_eq!(crypto.encrypt_calls.load(Ordering::SeqCst), 0);

    let output = EncryptedPayloadService::encrypt(
        &crypto,
        fence_with(1, 1, PayloadEncryption::Nip44V2, PayloadLimits::new(1, 2)),
        TransientPlaintext::new(b"ab".to_vec()),
    )
    .unwrap()
    .wait(Duration::from_millis(1));
    assert!(matches!(
        output,
        Err(PayloadError::CiphertextTooLarge { actual: 2, max: 1 })
    ));
}

#[test]
fn source_and_target_fence_reject_stale_results() {
    let crypto = ReadyCrypto::new();
    let limits = PayloadLimits::new(32, 32);
    let result = EncryptedPayloadService::decrypt(
        &crypto,
        fence_with(1, 4, PayloadEncryption::Nip04, limits),
        "secret".to_string(),
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
        fence_with(1, 4, PayloadEncryption::Nip04, limits),
        TransientPlaintext::new(b"secret".to_vec()),
    )
    .unwrap()
    .wait(Duration::from_millis(1))
    .unwrap();
    assert!(matches!(
        result.accept(fence(1, 5)),
        Err(StalePayloadResult)
    ));
}

#[test]
fn fence_distinguishes_absent_source_and_retired_crypto_policy() {
    let crypto = ReadyCrypto::new();
    let limits = PayloadLimits::new(32, 32);
    let old = PayloadFence::new(
        PayloadSource::Absent,
        [9; 32],
        4,
        [7; 32],
        PayloadPolicy::new(
            [5; 16],
            1,
            PayloadCodecId::new([6; 16]),
            PayloadEncryption::Nip04,
            peer(),
        ),
        limits,
    );
    let current = PayloadFence::new(
        PayloadSource::Absent,
        [9; 32],
        4,
        [7; 32],
        PayloadPolicy::new(
            [5; 16],
            2,
            PayloadCodecId::new([6; 16]),
            PayloadEncryption::Nip44V2,
            peer(),
        ),
        limits,
    );
    let result = EncryptedPayloadService::decrypt(&crypto, old, "secret".to_string())
        .unwrap()
        .wait(Duration::from_millis(1))
        .unwrap();

    assert!(matches!(result.accept(current), Err(StalePayloadResult)));
}

struct PendingDecrypt {
    sender: std::sync::Mutex<Option<crate::PendingSignerSender<TransientPlaintext>>>,
    cancelled: Arc<AtomicBool>,
}

struct PendingEncrypt {
    sender: Mutex<Option<crate::PendingSignerSender<EncryptedPayload>>>,
    cancelled: Arc<AtomicBool>,
}

impl EncryptCapability for PendingEncrypt {
    fn encrypt(&self, _request: EncryptPayloadRequest) -> SignerOp<EncryptedPayload> {
        let cancelled = Arc::clone(&self.cancelled);
        let (sender, operation) = SignerOp::pending_channel_with_cancel(move || {
            cancelled.store(true, Ordering::SeqCst);
        });
        *self.sender.lock().unwrap() = Some(sender);
        operation
    }
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
    let operation =
        EncryptedPayloadService::decrypt(&crypto, fence(1, 1), "ciphertext".to_string()).unwrap();
    let sender = crypto.sender.lock().unwrap().take().unwrap();

    drop(operation);
    assert!(cancelled.load(Ordering::SeqCst));
    assert!(matches!(
        sender.resolve(Ok(TransientPlaintext::new(b"late".to_vec()))),
        Err(PendingSignerResolveError::ReceiverDropped(Ok(_)))
    ));
}

#[test]
fn dropped_encrypt_cancels_and_late_ciphertext_is_refused() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let crypto = PendingEncrypt {
        sender: Mutex::new(None),
        cancelled: Arc::clone(&cancelled),
    };
    let operation = EncryptedPayloadService::encrypt(
        &crypto,
        fence(1, 1),
        TransientPlaintext::new(b"plaintext".to_vec()),
    )
    .unwrap();
    let sender = crypto.sender.lock().unwrap().take().unwrap();

    drop(operation);
    assert!(cancelled.load(Ordering::SeqCst));
    assert!(matches!(
        sender.resolve(Ok(EncryptedPayload::new("late".to_string()))),
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
    let exact_fence = fence_with(1, 1, PayloadEncryption::Nip44V2, PayloadLimits::new(1, 1));
    let result = EncryptedPayloadService::decrypt(capability, exact_fence, String::new())
        .unwrap()
        .wait(Duration::from_millis(1))
        .unwrap()
        .accept(exact_fence)
        .unwrap();
    assert!(result.is_empty());
}

#[test]
fn generic_envelope_has_no_plaintext_formatting_or_serialization_path() {
    let source = [
        include_str!("../payload.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source"),
        include_str!("../payload/service.rs"),
    ]
    .join("\n")
    .lines()
    .filter(|line| !line.trim_start().starts_with("///"))
    .collect::<Vec<_>>()
    .join("\n");
    for forbidden in [
        "impl Clone for TransientPlaintext",
        "impl Debug for TransientPlaintext",
        "impl Display for TransientPlaintext",
        "Serialize for TransientPlaintext",
        "println!",
        "eprintln!",
        "tracing::",
        "log::",
    ] {
        assert!(
            !source.contains(forbidden),
            "generic plaintext envelope acquired forbidden surface: {forbidden}"
        );
    }
}
