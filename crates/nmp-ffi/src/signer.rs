//! The app-supplied signer door across UniFFI (#1238).
//!
//! Before this module a Swift or Kotlin app could register no signer at all.
//! `nmp::Engine::add_signer` is generic over a Rust trait whose `sign` returns
//! a poll-thunk, so neither its parameter nor its result can cross this
//! boundary, and nothing else registered a signing capability. The only
//! identity such an app could give NMP was a raw secret key handed to
//! `add_account` — which is why 29er-next shipped a plaintext `nsec` in its
//! sandbox and two paragraphs of apology in its identity sheet.
//!
//! The door is deliberately NOT a `callback_interface`. The only one on this
//! surface is the AUTH policy bridge, and #783 exists to invert it: NMP must
//! not invoke app code. A signer is the capability where that matters most —
//! answering takes exactly as long as a person takes to approve something on
//! a hardware device or a phone, and a capability NMP calls into is one that
//! can freeze the caller for that long. So the app receives a stream of
//! immutable requests it drains on its own executor, and NMP calls nothing.

use std::sync::{Arc, Mutex};

use crate::convert::{parse_pubkey, FfiError};

/// One unsigned NIP-01 event the engine needs a signature for.
///
/// Distinct from [`crate::types::FfiSignEventRequest`], which travels the
/// other way and deliberately omits the author because NMP freezes it. Here
/// the author is already frozen and is the load-bearing field: it states which
/// key must produce this signature, and the promotion boundary rejects a
/// result signed by anyone else.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiUnsignedEvent {
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub content: String,
}

/// The closed set of refusals an APP can give for one signature request.
///
/// Deliberately narrower than the engine's full `SignerError`: `Timeout`,
/// `Disconnected` and `InvalidResponse` are determinations NMP makes about a
/// signer, not answers a signer gives about itself, so they are not offered
/// here. Every variant below has a real caller-side meaning.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiSignerRejection {
    /// The person said no. Terminal for the write — retrying cannot change a
    /// decision somebody already made.
    Rejected { reason: String },
    /// The signer cannot answer right now (locked device, disconnected
    /// bunker, app backgrounded). Retryable: the write parks and waits.
    Unavailable,
}

/// Settling a request the engine had already stopped awaiting.
///
/// One variant, because the completion door reports exactly one fact: its
/// single result slot was spent before this answer arrived. Cancellation and a
/// vanished engine-side waiter reach it indistinguishably, and inventing two
/// variants would claim a distinction that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiSignatureSettleError {
    /// The request was cancelled, or the write that asked for it went away.
    /// The answer is discarded; the mailbox is unaffected.
    NoLongerAwaited,
    /// This request was already settled. `resolve` and `reject` each spend the
    /// request exactly once — Rust enforces that by consuming it, and this is
    /// the same guarantee for callers that hold an object reference instead.
    AlreadySettled,
    /// The answer's `id`, `pubkey` or `sig` is not the fixed-width hex the
    /// protocol defines, so no signed event could be built from it. The
    /// request is NOT spent: correct the value and settle again.
    MalformedSignedEvent { reason: String },
}

impl std::fmt::Display for FfiSignatureSettleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLongerAwaited => {
                f.write_str("the engine was no longer awaiting this signature request")
            }
            Self::AlreadySettled => f.write_str("this signature request was already settled"),
            Self::MalformedSignedEvent { reason } => {
                write!(f, "the signed event could not be parsed: {reason}")
            }
        }
    }
}

impl std::error::Error for FfiSignatureSettleError {}

/// One signature the engine is waiting for, handed to an app-owned signer.
///
/// Settles exactly once. In Rust that is the type's own property — `resolve`
/// and `reject` consume the request — but an object reference crossing UniFFI
/// cannot be consumed, so the take-once slot is an `Option` this side takes
/// from. A second settle is [`FfiSignatureSettleError::AlreadySettled`],
/// never a second answer reaching the engine.
///
/// Dropping this object without settling is a legal answer: the engine hears
/// the ordinary retryable unavailable and the write parks, which is what an
/// app whose signer went away should say.
#[derive(uniffi::Object)]
pub struct NmpSignatureRequest {
    /// `Option::take` rather than a settled flag beside a live value: after
    /// the take there is nothing left to settle twice (Bool-Lifecycle Gate).
    inner: Mutex<Option<nmp::SignatureRequest>>,
    unsigned: FfiUnsignedEvent,
}

impl NmpSignatureRequest {
    fn new(request: nmp::SignatureRequest) -> Arc<Self> {
        let unsigned = unsigned_event_to_ffi(request.unsigned_event());
        Arc::new(Self {
            inner: Mutex::new(Some(request)),
            unsigned,
        })
    }

    fn take(&self) -> Result<nmp::SignatureRequest, FfiSignatureSettleError> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
            .ok_or(FfiSignatureSettleError::AlreadySettled)
    }
}

#[uniffi::export]
impl NmpSignatureRequest {
    /// The exact body to sign. Its `pubkey` is frozen: sign as that key or
    /// refuse, never as another.
    pub fn unsigned_event(&self) -> FfiUnsignedEvent {
        self.unsigned.clone()
    }

    /// Answer with a signature. The engine verifies the returned event against
    /// the frozen template — signature, id, author, timestamp, kind, tags,
    /// content — before it can reach a relay, so a wrong answer fails the
    /// write rather than publishing something the app did not mean.
    ///
    /// Malformed hex in `signed` is
    /// [`FfiSignatureSettleError::MalformedSignedEvent`] and does NOT spend
    /// the request: the app can correct it and settle again.
    ///
    /// The parameter is `event` rather than `signed` because `signed` is a C
    /// keyword: UniFFI spells parameter names straight into the generated C
    /// header, where `RustBuffer signed` does not compile.
    pub fn resolve(
        &self,
        event: crate::types::FfiSignedEvent,
    ) -> Result<(), FfiSignatureSettleError> {
        // Parse BEFORE taking, so a malformed answer is a correctable mistake
        // rather than a request the app has irrevocably burned.
        let parsed = signed_event_from_ffi(event)?;
        self.take()?.resolve(parsed).map_err(Into::into)
    }

    /// Answer with a refusal.
    pub fn reject(&self, reason: FfiSignerRejection) -> Result<(), FfiSignatureSettleError> {
        let reason = match reason {
            FfiSignerRejection::Rejected { reason } => nmp::SignerError::Rejected(reason),
            FfiSignerRejection::Unavailable => nmp::SignerError::Unavailable,
        };
        self.take()?.reject(reason).map_err(Into::into)
    }
}

/// The app's end of one registered signer: the stream of signature requests to
/// drain, and the exact-instance proof that removes the registration.
///
/// This object IS the registration. There is no separate registration record,
/// because there is nothing a second object could prove that holding this one
/// does not — and a stale copy could not detach a replacement either way,
/// since removal carries the exact instance the engine registered.
#[derive(uniffi::Object)]
pub struct NmpSignerMailbox {
    mailbox: nmp::SignerMailbox,
    registration: nmp::SignerRegistration,
    public_key: String,
}

impl NmpSignerMailbox {
    pub(crate) fn new(
        mailbox: nmp::SignerMailbox,
        registration: nmp::SignerRegistration,
        public_key: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            mailbox,
            registration,
            public_key,
        })
    }

    pub(crate) fn registration(&self) -> nmp::SignerRegistration {
        self.registration.clone()
    }
}

#[uniffi::export]
impl NmpSignerMailbox {
    /// The key this mailbox signs for.
    pub fn public_key(&self) -> String {
        self.public_key.clone()
    }

    /// Await the next signature request, or `None` once the mailbox is closed
    /// and drained. A second concurrent `next()` is
    /// [`FfiError::ConcurrentNext`] — one mailbox is one drainer, because two
    /// would each believe they held the only copy of a take-once completion.
    pub async fn next(&self) -> Result<Option<Arc<NmpSignatureRequest>>, FfiError> {
        match self.mailbox.next().await {
            Ok(Some(request)) => Ok(Some(NmpSignatureRequest::new(request))),
            Ok(None) => Ok(None),
            Err(_) => Err(FfiError::ConcurrentNext),
        }
    }

    /// Stop accepting requests and wake a parked [`Self::next`] to `None`.
    /// Idempotent. This does not remove the registration — writes for this key
    /// then park on an unavailable signer, exactly as they do before any
    /// signer attaches. `NmpEngine::remove_signer_mailbox` removes it.
    pub fn cancel(&self) {
        self.mailbox.cancel();
    }
}

impl From<nmp::SignatureSettleError> for FfiSignatureSettleError {
    fn from(error: nmp::SignatureSettleError) -> Self {
        match error {
            nmp::SignatureSettleError::NoLongerAwaited => Self::NoLongerAwaited,
        }
    }
}

fn unsigned_event_to_ffi(unsigned: &nmp::SignerUnsignedEvent) -> FfiUnsignedEvent {
    FfiUnsignedEvent {
        pubkey: hex_lower(unsigned.public_key().as_bytes()),
        created_at: unsigned.created_at(),
        kind: unsigned.kind(),
        tags: unsigned.tags().to_vec(),
        content: unsigned.content().to_string(),
    }
}

/// Parse an app's signed answer into the protocol-neutral signer value.
///
/// Tags pass through as the raw strings they already are: this boundary does
/// not re-model them, because the engine's promotion boundary compares the
/// whole returned event against the frozen template anyway, and a tag shape
/// this layer "corrected" would silently fail that comparison instead of
/// being reported.
fn signed_event_from_ffi(
    signed: crate::types::FfiSignedEvent,
) -> Result<nmp::SignerSignedEvent, FfiSignatureSettleError> {
    let malformed = |field: &str, got: &str| FfiSignatureSettleError::MalformedSignedEvent {
        reason: format!("{field} is not 32-byte hex: {got}"),
    };
    let id = decode_hex_32(&signed.id).ok_or_else(|| malformed("id", &signed.id))?;
    let public_key = parse_pubkey(&signed.pubkey).map_err(|_| {
        FfiSignatureSettleError::MalformedSignedEvent {
            reason: format!("pubkey is not a valid public key: {}", signed.pubkey),
        }
    })?;
    let signature = decode_hex_64(&signed.sig).ok_or_else(|| {
        FfiSignatureSettleError::MalformedSignedEvent {
            reason: format!("sig is not 64-byte hex: {}", signed.sig),
        }
    })?;

    Ok(nmp::SignerSignedEvent::new(
        id,
        nmp::SignerPublicKey::new(public_key.to_bytes()),
        signed.created_at,
        signed.kind,
        signed.tags,
        signed.content,
        signature,
    ))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex_32(text: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    decode_hex_into(text, &mut out).then_some(out)
}

fn decode_hex_64(text: &str) -> Option<[u8; 64]> {
    let mut out = [0u8; 64];
    decode_hex_into(text, &mut out).then_some(out)
}

fn decode_hex_into(text: &str, out: &mut [u8]) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != out.len() * 2 {
        return false;
    }
    for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        match (hex_nibble(pair[0]), hex_nibble(pair[1])) {
            (Some(high), Some(low)) => *slot = (high << 4) | low,
            _ => return false,
        }
    }
    true
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::{NmpEngine, NmpEngineConfig};
    use crate::types::{FfiSignEventRequest, FfiSignedEvent};

    /// A signer that lives entirely on the "app" side of this boundary, doing
    /// what a Swift signer would do: take the frozen unsigned event, produce a
    /// real signature over exactly those bytes, hand back the whole event.
    fn app_side_signature(unsigned: &FfiUnsignedEvent, keys: &nostr::Keys) -> FfiSignedEvent {
        let tags: Vec<nostr::Tag> = unsigned
            .tags
            .iter()
            .map(|tag| nostr::Tag::parse(tag.clone()).expect("fixture tags parse"))
            .collect();
        let unsigned_event = nostr::UnsignedEvent::new(
            keys.public_key(),
            nostr::Timestamp::from(unsigned.created_at),
            nostr::Kind::from(unsigned.kind),
            tags,
            unsigned.content.clone(),
        );
        let signed = unsigned_event.sign_with_keys(keys).expect("fixture signs");
        FfiSignedEvent {
            id: signed.id.to_hex(),
            pubkey: signed.pubkey.to_hex(),
            created_at: signed.created_at.as_secs(),
            kind: signed.kind.as_u16(),
            tags: signed
                .tags
                .iter()
                .map(|tag| tag.clone().to_vec())
                .collect(),
            content: signed.content.clone(),
            sig: signed.sig.to_string(),
        }
    }

    fn engine() -> Arc<NmpEngine> {
        NmpEngine::new(NmpEngineConfig::default()).expect("in-memory engine must build")
    }

    /// The headline this issue exists for: an app that holds no secret NMP
    /// knows about registers a signer and produces a real signature, entirely
    /// through the FFI surface a Swift app has.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ffi_an_app_supplied_signer_signs_through_its_mailbox() {
        let engine = engine();
        let keys = nostr::Keys::generate();
        let pubkey = keys.public_key().to_hex();

        let mailbox = engine
            .add_signer_mailbox(pubkey.clone())
            .expect("a public key is all this door needs");
        assert_eq!(mailbox.public_key(), pubkey);
        engine.set_active_account(Some(pubkey.clone())).unwrap();

        let handle = engine
            .sign_event(FfiSignEventRequest {
                created_at: 5,
                kind: 1,
                tags: Vec::new(),
                content: "signed by the app".to_string(),
            })
            .expect("the operation is admitted");

        let request = mailbox
            .next()
            .await
            .expect("the mailbox is open")
            .expect("the engine asked this signer for something");
        let unsigned = request.unsigned_event();
        assert_eq!(unsigned.pubkey, pubkey, "the author is frozen in the request");
        assert_eq!(unsigned.content, "signed by the app");

        request
            .resolve(app_side_signature(&unsigned, &keys))
            .expect("the engine is still waiting for this");

        let signed = handle.signed().await.expect("the app's signature is accepted");
        assert_eq!(signed.pubkey, pubkey);
        assert_eq!(signed.content, "signed by the app");
    }

    /// A malformed answer is reported and the request survives it. An app that
    /// sends bad hex has made a correctable mistake, not burned the one
    /// signature the engine was waiting for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ffi_a_malformed_answer_does_not_spend_the_request() {
        let engine = engine();
        let keys = nostr::Keys::generate();
        let pubkey = keys.public_key().to_hex();

        let mailbox = engine.add_signer_mailbox(pubkey.clone()).unwrap();
        engine.set_active_account(Some(pubkey.clone())).unwrap();
        let handle = engine
            .sign_event(FfiSignEventRequest {
                created_at: 5,
                kind: 1,
                tags: Vec::new(),
                content: "retry after a bad answer".to_string(),
            })
            .unwrap();

        let request = mailbox.next().await.unwrap().unwrap();
        let good = app_side_signature(&request.unsigned_event(), &keys);

        let mut bad = good.clone();
        bad.sig = "not hex".to_string();
        assert!(
            matches!(
                request.resolve(bad),
                Err(FfiSignatureSettleError::MalformedSignedEvent { .. })
            ),
            "a malformed signature must be reported, not accepted"
        );

        // The request is still live, so the app can correct itself.
        request.resolve(good).expect("the corrected answer is taken");
        let signed = handle.signed().await.expect("the corrected answer is accepted");
        assert_eq!(signed.content, "retry after a bad answer");
    }

    /// Take-once, held across a boundary that cannot consume a value. A second
    /// settle is refused rather than delivering a second answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ffi_a_request_settles_exactly_once() {
        let engine = engine();
        let keys = nostr::Keys::generate();
        let pubkey = keys.public_key().to_hex();

        let mailbox = engine.add_signer_mailbox(pubkey.clone()).unwrap();
        engine.set_active_account(Some(pubkey.clone())).unwrap();
        let handle = engine
            .sign_event(FfiSignEventRequest {
                created_at: 5,
                kind: 1,
                tags: Vec::new(),
                content: "settled once".to_string(),
            })
            .unwrap();

        let request = mailbox.next().await.unwrap().unwrap();
        request
            .resolve(app_side_signature(&request.unsigned_event(), &keys))
            .expect("the first settle is the answer");
        assert_eq!(
            request.reject(FfiSignerRejection::Unavailable),
            Err(FfiSignatureSettleError::AlreadySettled),
            "a settled request has no second answer to give"
        );

        handle.signed().await.expect("the first answer stands");
    }

    /// An app's refusal reaches the caller as a rejection, not as a hang or a
    /// timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ffi_an_app_refusal_reaches_the_caller() {
        let engine = engine();
        let keys = nostr::Keys::generate();
        let pubkey = keys.public_key().to_hex();

        let mailbox = engine.add_signer_mailbox(pubkey.clone()).unwrap();
        engine.set_active_account(Some(pubkey.clone())).unwrap();
        let handle = engine
            .sign_event(FfiSignEventRequest {
                created_at: 5,
                kind: 1,
                tags: Vec::new(),
                content: "the user declines".to_string(),
            })
            .unwrap();

        let request = mailbox.next().await.unwrap().unwrap();
        request
            .reject(FfiSignerRejection::Rejected {
                reason: "user declined".to_string(),
            })
            .expect("the engine is still waiting");

        let outcome = handle.signed().await;
        assert!(
            matches!(
                outcome,
                Err(crate::types::FfiSignEventFailure::SignerRejected { .. })
            ),
            "got {outcome:?}"
        );
    }

    /// The mailbox is the exact-instance registration proof, with the same
    /// stale-safety every other registration on this surface has.
    #[test]
    fn ffi_signer_mailbox_registration_is_stale_safe() {
        let engine = engine();
        let pubkey = nostr::Keys::generate().public_key().to_hex();

        let first = engine.add_signer_mailbox(pubkey.clone()).unwrap();
        let replacement = engine.add_signer_mailbox(pubkey.clone()).unwrap();
        assert_eq!(first.public_key(), replacement.public_key());

        assert!(
            !engine.remove_signer_mailbox(Arc::clone(&first)).unwrap(),
            "the superseded mailbox must not detach its replacement"
        );
        assert!(engine
            .remove_signer_mailbox(Arc::clone(&replacement))
            .unwrap());
        assert!(!engine.remove_signer_mailbox(replacement).unwrap());
    }

    /// A public key is the only thing this door accepts, and a malformed one
    /// is a typed synchronous refusal that registers nothing.
    #[test]
    fn ffi_a_malformed_public_key_registers_nothing() {
        let engine = engine();
        assert!(matches!(
            engine.add_signer_mailbox("nope".to_string()),
            Err(FfiError::InvalidPublicKey { .. })
        ));
    }
}
