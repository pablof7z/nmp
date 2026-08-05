//! The app-supplied signer door (#1238), in the inverted shape #783 requires.
//!
//! A signing capability written in Rust is an ordinary [`SigningCapability`]
//! handed to [`Engine::add_signer`](crate::Engine::add_signer). A signing
//! capability written in Swift or Kotlin cannot be: the trait is generic and
//! its `sign` returns a poll-thunk, neither of which crosses UniFFI. Before
//! this module the consequence was total — an app on those platforms could
//! register no signer at all, so the only identity NMP could hold for it was
//! a local secret key NMP itself owns.
//!
//! The door is not a callback. #783's required contract is that NMP never
//! invokes app code: a signer legitimately takes as long as a person takes to
//! tap "approve" on a hardware device, and a capability NMP calls into is a
//! capability that can freeze the caller. So registration installs an
//! engine-owned bounded mailbox instead. [`MailboxSigner::sign`] enqueues one
//! immutable [`SignatureRequest`] and returns immediately; the app drains the
//! mailbox on its own executor and settles each request through a take-once
//! completion. If the app never drains, only that signer's own queued work
//! waits — NMP creates no thread and runs no foreign code.
//!
//! Saturation is a refusal, not a broken mailbox. `MailboxSigner` counts its
//! own outstanding requests and answers the one past the bound with
//! [`SignerError::Unavailable`] — the ordinary retryable "no usable signer
//! session right now" every other signer already reports — rather than
//! letting the underlying queue latch a terminal lag state. The mailbox
//! survives, and the write parks exactly as it does for a signer that has not
//! attached yet (`features/identity/awaiting-signer.feature`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use nmp_signer::{
    PendingSignerResolveError, PendingSignerSender, SignerError, SignerOp, SignerPublicKey,
    SignerSignedEvent, SignerUnsignedEvent, SigningCapability,
};

use crate::runtime::{fifo_channel, AsyncFifoReceiver, ConcurrentNext, FifoNextError, FifoSender};

/// How many signature requests one mailbox may hold outstanding at once.
///
/// Deliberately the same fixed bound the engine's other app-facing FIFOs use,
/// and deliberately not configurable: a public capacity knob is one of the
/// things #783 forbids. Outstanding means queued OR taken-but-unsettled, so
/// an app that drains without settling saturates exactly as fast as one that
/// never drains at all.
const OUTSTANDING_REQUEST_BOUND: usize = crate::runtime::FACT_CHANNEL_CAPACITY;

/// Shared count of one mailbox's outstanding requests.
///
/// Held by the capability (which increments on enqueue) and by every live
/// [`SignatureRequest`] (which decrements exactly once, on settle or drop).
/// A slot is therefore released by the request's own lifetime rather than by
/// anything the app is obliged to remember to call.
type OutstandingSlots = Arc<AtomicUsize>;

/// Settling a [`SignatureRequest`] that the engine had already stopped
/// awaiting.
///
/// Exactly one variant, because the completion door reports exactly one fact:
/// its single result slot was spent before this answer arrived. Cancellation
/// and a dropped engine-side waiter both arrive here indistinguishably, and
/// inventing two variants would claim a distinction this door cannot make.
/// The answer is discarded; nothing else about the mailbox is affected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureSettleError {
    /// The request was cancelled, or the write that asked for it went away,
    /// before this answer arrived.
    NoLongerAwaited,
}

impl std::fmt::Display for SignatureSettleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLongerAwaited => {
                f.write_str("the engine was no longer awaiting this signature request")
            }
        }
    }
}

impl std::error::Error for SignatureSettleError {}

/// One signature the engine needs, handed to an app-owned signer.
///
/// The request is immutable and settles exactly once. `resolve` and `reject`
/// both consume it, so the take-once property is the type's, not a rule the
/// app has to keep: there is no second call to make. Dropping it without
/// settling is a legal answer too — it reports the same retryable
/// [`SignerError::Unavailable`] as an unattached signer, which is what an app
/// that shut its signer down mid-request should say.
pub struct SignatureRequest {
    unsigned: SignerUnsignedEvent,
    completion: PendingSignerSender<SignerSignedEvent>,
    slots: OutstandingSlots,
    /// Cleared by `settle`, so `Drop` can tell an answered request from an
    /// abandoned one without a separate bool.
    settled: bool,
}

impl SignatureRequest {
    /// The exact event body to sign. The author is already frozen into it by
    /// the accepting write, so a signer's only job is to produce a signature
    /// over these bytes — never to choose a different identity.
    #[must_use]
    pub fn unsigned_event(&self) -> &SignerUnsignedEvent {
        &self.unsigned
    }

    /// Answer with a signature. The engine verifies the returned event against
    /// the frozen template (signature, id, author, timestamp, kind, tags,
    /// content) before it can reach a relay, so a wrong or forged answer fails
    /// the write rather than publishing.
    pub fn resolve(mut self, signed: SignerSignedEvent) -> Result<(), SignatureSettleError> {
        self.settle(Ok(signed))
    }

    /// Answer with a refusal. [`SignerError::Rejected`] is terminal for the
    /// write — the person said no and retrying cannot change it — while
    /// [`SignerError::Unavailable`] parks it for a later attempt.
    pub fn reject(mut self, reason: SignerError) -> Result<(), SignatureSettleError> {
        self.settle(Err(reason))
    }

    fn settle(
        &mut self,
        outcome: Result<SignerSignedEvent, SignerError>,
    ) -> Result<(), SignatureSettleError> {
        self.settled = true;
        self.slots.fetch_sub(1, Ordering::AcqRel);
        match self.completion.resolve(outcome) {
            Ok(()) => Ok(()),
            // The one completion door was spent before this answer arrived —
            // by cancellation, or by the engine-side waiter disappearing. The
            // door does not distinguish them and neither does this error.
            Err(PendingSignerResolveError::ReceiverDropped(_))
            | Err(PendingSignerResolveError::AlreadyResolved(_)) => {
                Err(SignatureSettleError::NoLongerAwaited)
            }
        }
    }
}

impl Drop for SignatureRequest {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        // An abandoned request must not hold its slot forever, and must not
        // leave the write waiting on an answer that will never come.
        self.slots.fetch_sub(1, Ordering::AcqRel);
        let _ = self.completion.resolve(Err(SignerError::Unavailable));
    }
}

impl std::fmt::Debug for SignatureRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignatureRequest")
            .field("kind", &self.unsigned.kind())
            .field("public_key", &self.unsigned.public_key())
            .finish_non_exhaustive()
    }
}

/// The app's end of one registered signer: a stream of signature requests to
/// drain on the app's own executor.
///
/// Dropping or [`cancel`](Self::cancel)ling it does not remove the registration —
/// [`Engine::remove_signer`](crate::Engine::remove_signer) does that, with the
/// exact registration proof. A closed mailbox simply answers every subsequent
/// request as unavailable, which parks writes for that key instead of failing
/// them.
pub struct SignerMailbox {
    requests: AsyncFifoReceiver<SignatureRequest>,
}

impl SignerMailbox {
    /// Await the next signature request, or `None` once the mailbox is closed
    /// and drained.
    ///
    /// Exactly one `next()` may be outstanding at a time. An overlapping call
    /// is [`ConcurrentNext`] rather than a silently lost request — two
    /// drainers would each believe they held the only copy of a take-once
    /// completion.
    pub async fn next(&self) -> Result<Option<SignatureRequest>, ConcurrentNext> {
        match self.requests.next().await {
            Ok(request) => Ok(request),
            Err(FifoNextError::ConcurrentNext) => Err(ConcurrentNext),
            // Structurally unreachable: `MailboxSigner::sign` refuses past the
            // bound before the queue can ever fill, so it cannot latch a lag
            // state. Ending the stream is the safe fallback if that invariant
            // is ever broken — never a panic in an app's signing path.
            Err(FifoNextError::Lagged) => Ok(None),
        }
    }

    /// Stop accepting requests and wake a parked [`next`](Self::next) to
    /// `None`. Idempotent. Spelled `cancel` to match every other pull handle
    /// on this surface, and because `close` is already taken by the object
    /// lifecycle UniFFI generates for the FFI projection.
    pub fn cancel(&self) {
        self.requests.close();
    }
}

/// The [`SigningCapability`] half of a mailbox registration.
///
/// It is a real capability in the engine's one signer registry — nothing about
/// the registry, the promotion boundary, or the parked-write machinery knows
/// this signer is reached through a mailbox rather than implemented in Rust.
pub struct MailboxSigner {
    public_key: SignerPublicKey,
    requests: FifoSender<SignatureRequest>,
    slots: OutstandingSlots,
}

impl SigningCapability for MailboxSigner {
    fn public_key(&self) -> Option<SignerPublicKey> {
        Some(self.public_key)
    }

    fn sign(&self, unsigned: SignerUnsignedEvent) -> SignerOp<SignerSignedEvent> {
        // Claim a slot before building anything, so the bound is enforced by
        // the claim rather than by the queue's own overflow behaviour (which
        // latches a terminal lag state this door must never reach).
        let claimed = self
            .slots
            .try_update(Ordering::AcqRel, Ordering::Acquire, |outstanding| {
                (outstanding < OUTSTANDING_REQUEST_BOUND).then_some(outstanding + 1)
            });
        if claimed.is_err() {
            return SignerOp::err(SignerError::Unavailable);
        }

        let (completion, operation) = SignerOp::pending_channel();
        let request = SignatureRequest {
            unsigned,
            completion,
            slots: Arc::clone(&self.slots),
            settled: false,
        };
        // `send` returns false only for a closed/gone consumer. Dropping the
        // request then releases the slot and resolves the operation as
        // unavailable through `SignatureRequest::drop`, so a closed mailbox
        // parks writes rather than stranding them.
        let _ = self.requests.send(request);
        operation
    }
}

/// Build one mailbox registration's two halves for `public_key`.
pub(crate) fn mailbox_signer(public_key: SignerPublicKey) -> (MailboxSigner, SignerMailbox) {
    let (requests, receiver) = fifo_channel();
    (
        MailboxSigner {
            public_key,
            requests,
            slots: Arc::new(AtomicUsize::new(0)),
        },
        SignerMailbox {
            requests: receiver.into_async(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsigned(public_key: SignerPublicKey, content: &str) -> SignerUnsignedEvent {
        SignerUnsignedEvent::new(public_key, 0, 1, Vec::new(), content.to_string())
    }

    fn fixture_key() -> SignerPublicKey {
        SignerPublicKey::new([7u8; 32])
    }

    /// The refusal that keeps a mailbox alive (#783 falsifier 2). An app that
    /// registers a signer and then never drains it must not be able to wedge
    /// the engine, and must not permanently break its own mailbox either: the
    /// queue stays at its bound, the overflow is the ordinary retryable
    /// unavailable answer, and draining one request makes room for one more.
    #[tokio::test]
    async fn an_undrained_mailbox_refuses_past_its_bound_and_still_works_afterwards() {
        let key = fixture_key();
        let (signer, mailbox) = mailbox_signer(key);

        // Fill it exactly to the bound. Every one of these is retained.
        let queued: Vec<_> = (0..OUTSTANDING_REQUEST_BOUND)
            .map(|n| signer.sign(unsigned(key, &format!("queued {n}"))))
            .collect();
        assert!(queued.iter().all(|op| matches!(op, SignerOp::Pending(_))));

        // The one past the bound is refused, not queued, not dropped.
        let refused = signer.sign(unsigned(key, "over the bound"));
        assert!(
            matches!(refused, SignerOp::Ready(Err(SignerError::Unavailable))),
            "a saturated mailbox must answer the ordinary retryable unavailable"
        );

        // The mailbox is not broken by having been saturated: it still
        // delivers the backlog in order.
        let first = mailbox
            .next()
            .await
            .expect("a saturated mailbox is still readable")
            .expect("the backlog is still there");
        assert_eq!(first.unsigned_event().content(), "queued 0");

        // Settling that one returns its slot, so the signer accepts again.
        first.reject(SignerError::Unavailable).ok();
        assert!(
            matches!(signer.sign(unsigned(key, "after draining")), SignerOp::Pending(_)),
            "settling a request must return its slot to the bound"
        );
    }

    /// An app that takes a request and then abandons it — its signer process
    /// died, its task was cancelled — must not leave the write waiting
    /// forever, and must not leak the slot it claimed.
    #[tokio::test]
    async fn an_abandoned_request_answers_unavailable_and_releases_its_slot() {
        let key = fixture_key();
        let (signer, mailbox) = mailbox_signer(key);

        let operation = signer.sign(unsigned(key, "abandon me"));
        let request = mailbox.next().await.unwrap().unwrap();
        drop(request);

        assert_eq!(
            operation.recv_async().await,
            Err(SignerError::Unavailable),
            "dropping a request must answer the write, not strand it"
        );
        assert_eq!(
            signer.slots.load(Ordering::Acquire),
            0,
            "an abandoned request must not leak its slot"
        );
    }

    /// Rejecting is a real answer with the app's own reason, and a terminal
    /// one: `Rejected` is how a person saying no reaches the write.
    #[tokio::test]
    async fn a_rejection_carries_the_apps_own_terminal_reason() {
        let key = fixture_key();
        let (signer, mailbox) = mailbox_signer(key);

        let operation = signer.sign(unsigned(key, "ask the user"));
        let request = mailbox.next().await.unwrap().unwrap();
        request
            .reject(SignerError::Rejected("user declined".to_string()))
            .expect("the engine is still awaiting this one");

        let outcome = operation.recv_async().await;
        assert_eq!(outcome, Err(SignerError::Rejected("user declined".into())));
        assert!(
            outcome.unwrap_err().is_terminal(),
            "a person declining is terminal for the write, not a retry"
        );
    }

    /// Settling a request the engine stopped awaiting is reported, not
    /// panicked and not silently swallowed — and it still frees the slot.
    #[tokio::test]
    async fn settling_an_unawaited_request_is_reported() {
        let key = fixture_key();
        let (signer, mailbox) = mailbox_signer(key);

        let operation = signer.sign(unsigned(key, "nobody waits for this"));
        let request = mailbox.next().await.unwrap().unwrap();
        drop(operation);

        assert_eq!(
            request.reject(SignerError::Unavailable),
            Err(SignatureSettleError::NoLongerAwaited)
        );
        assert_eq!(signer.slots.load(Ordering::Acquire), 0);
    }

    /// Closing the mailbox is the app saying "I am no longer this signer".
    /// Writes for that key park on an unavailable answer rather than failing,
    /// which is the same thing an unattached signer does.
    #[tokio::test]
    async fn a_closed_mailbox_parks_writes_instead_of_stranding_them() {
        let key = fixture_key();
        let (signer, mailbox) = mailbox_signer(key);
        mailbox.cancel();

        let operation = signer.sign(unsigned(key, "after close"));
        assert_eq!(operation.recv_async().await, Err(SignerError::Unavailable));
        assert!(
            mailbox.next().await.expect("close is not an error").is_none(),
            "a closed, drained mailbox ends its stream"
        );
    }

    /// The registered capability reports exactly the key it was built for, so
    /// the engine's registry keys it correctly and the promotion boundary has
    /// something to check the returned author against.
    #[test]
    fn the_capability_reports_the_key_it_was_registered_for() {
        let key = fixture_key();
        let (signer, _mailbox) = mailbox_signer(key);
        assert_eq!(signer.public_key(), Some(key));
    }
}
