//! Facade-owned NIP-42 AUTH policy surface (#8 Wave 5).
//!
//! The app-facing half of the runtime capability registry: an app installs
//! one [`AuthPolicy`] per expected account identity via
//! [`Engine::add_auth_policy`](crate::Engine::add_auth_policy), and the
//! engine consults it — nonblocking, ready-or-pending — every time a relay
//! challenges one exact protected session.
//!
//! Every type here is a newtype over runtime machinery, including the two
//! semantic answer types: [`AuthPolicyDecision`] and [`AuthPolicyError`] are
//! defined by [`nmp_runtime`], which is what evaluates a policy and
//! produces them, and are re-exported through this facade for apps to name.
//! [`AuthPolicyRequest`], [`AuthPolicyOp`], and [`AuthPolicyPendingSender`]
//! are newtypes over runtime request and cancellation machinery. The
//! pending/cancel linearization (a losing terminal-cancel waits for a
//! resolver that already owns completion; a queued answer is consumed
//! before the receiver may be abandoned) is runtime-owned and inherited by
//! delegation — nothing here re-implements a channel, a cancel handshake,
//! or any part of that race's resolution.

use nostr::{PublicKey, RelayUrl};

pub use nmp_runtime::{AuthPolicyDecision, AuthPolicyError, AuthPolicyResult};

/// Immutable input to one app-owned NIP-42 authorization decision: the
/// frozen expected identity, the exact canonical relay, the exact challenge
/// bytes, and the exact transport generation/epoch the decision is bound
/// to. A newer challenge, reconnect, or identity change invalidates the
/// operation this request started — a late answer is ignored by exact
/// session, generation, epoch, and token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthPolicyRequest {
    inner: nmp_runtime::AuthPolicyRequest,
}

impl AuthPolicyRequest {
    pub(crate) fn from_engine(inner: nmp_runtime::AuthPolicyRequest) -> Self {
        Self { inner }
    }

    /// The frozen expected pubkey this session authenticates AS. Never
    /// "whatever account is active when the operation finishes."
    #[must_use]
    pub fn expected_pubkey(&self) -> PublicKey {
        self.inner.expected_pubkey()
    }

    /// The exact canonical relay that issued the challenge.
    #[must_use]
    pub fn relay(&self) -> &RelayUrl {
        self.inner.relay()
    }

    /// The exact challenge bytes, preserved untrimmed.
    #[must_use]
    pub fn challenge(&self) -> &str {
        self.inner.challenge()
    }

    /// The transport generation this decision is bound to.
    #[must_use]
    pub fn transport_generation(&self) -> u64 {
        self.inner.transport_generation()
    }

    /// The challenge epoch this decision is bound to.
    #[must_use]
    pub fn epoch_sequence(&self) -> u64 {
        self.inner.epoch_sequence()
    }
}

/// One-shot completion door for a pending [`AuthPolicyOp`]. Cloneable;
/// exactly one clone's [`Self::resolve`] ever lands — every later call gets
/// the typed [`AuthPolicyResolveError`] carrying the undelivered result
/// back instead of silently dropping it.
#[derive(Clone)]
pub struct AuthPolicyPendingSender {
    inner: nmp_runtime::AuthPolicyPendingSender,
}

impl AuthPolicyPendingSender {
    /// Deliver the policy's answer. `Err(AlreadyResolved)` -- some clone
    /// already resolved this operation; `Err(ReceiverDropped)` -- the engine
    /// terminally cancelled it first (a newer challenge epoch, capability
    /// removal, session teardown, or shutdown). Both hand the result back.
    pub fn resolve(&self, result: AuthPolicyResult) -> Result<(), AuthPolicyResolveError> {
        self.inner
            .resolve(result)
            .map_err(AuthPolicyResolveError::from_runtime)
    }
}

/// Why an [`AuthPolicyPendingSender::resolve`] did not deliver. Carries the
/// exact undelivered result so a caller can observe (or log) what was lost.
#[derive(Debug)]
pub enum AuthPolicyResolveError {
    /// A clone of this sender already delivered an answer.
    AlreadyResolved(AuthPolicyResult),
    /// The engine terminally cancelled the operation before this answer.
    ReceiverDropped(AuthPolicyResult),
}

impl AuthPolicyResolveError {
    fn from_runtime(error: nmp_runtime::AuthPolicyResolveError) -> Self {
        match error {
            nmp_runtime::AuthPolicyResolveError::AlreadyResolved(result) => {
                Self::AlreadyResolved(result)
            }
            nmp_runtime::AuthPolicyResolveError::ReceiverDropped(result) => {
                Self::ReceiverDropped(result)
            }
        }
    }
}

/// Nonblocking ready-or-pending policy operation — what
/// [`AuthPolicy::evaluate`] returns. A strict newtype over the engine's
/// operation: the pending channel, the cancel handshake, and their
/// linearization live in the engine and are inherited by delegation.
pub struct AuthPolicyOp {
    inner: nmp_runtime::AuthPolicyOp,
}

impl AuthPolicyOp {
    pub(crate) fn into_engine(self) -> nmp_runtime::AuthPolicyOp {
        self.inner
    }

    /// An immediately-available answer.
    #[must_use]
    pub fn ready(result: AuthPolicyResult) -> Self {
        Self {
            inner: nmp_runtime::AuthPolicyOp::ready(result),
        }
    }

    /// Shorthand for `ready(Ok(AuthPolicyDecision::Allow))`.
    #[must_use]
    pub fn allow() -> Self {
        Self {
            inner: nmp_runtime::AuthPolicyOp::allow(),
        }
    }

    /// Shorthand for `ready(Ok(AuthPolicyDecision::Deny { reason }))`.
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            inner: nmp_runtime::AuthPolicyOp::deny(reason),
        }
    }

    /// A pending operation with no app-side cancellation hook. If the
    /// engine cancels first, the sender's `resolve` reports
    /// [`AuthPolicyResolveError::ReceiverDropped`].
    #[must_use]
    pub fn pending_channel() -> (AuthPolicyPendingSender, Self) {
        let (sender, op) = nmp_runtime::AuthPolicyOp::pending_channel();
        (
            AuthPolicyPendingSender { inner: sender },
            Self { inner: op },
        )
    }

    /// A pending operation whose `cancel` hook the engine invokes on
    /// terminal cancellation (newer epoch, capability removal, teardown,
    /// shutdown). The engine linearizes cancellation against completion: an
    /// answer already committed wins over the cancel, and a resolver that
    /// already owns completion keeps the door open until it sends.
    #[must_use]
    pub fn pending_channel_with_cancel(
        cancel: impl FnOnce() + Send + 'static,
    ) -> (AuthPolicyPendingSender, Self) {
        let (sender, op) = nmp_runtime::AuthPolicyOp::pending_channel_with_cancel(cancel);
        (
            AuthPolicyPendingSender { inner: sender },
            Self { inner: op },
        )
    }
}

/// App-owned NIP-42 authorization policy. `evaluate` must return a
/// ready-or-pending operation without blocking; the engine executes it on
/// its finite native executor and owns pending cancellation. Prompt UX
/// remains app-owned — resolve the pending sender whenever the user
/// answers, from any thread.
pub trait AuthPolicy: Send {
    fn evaluate(&self, request: AuthPolicyRequest) -> AuthPolicyOp;
}

/// The one crossing between the facade vocabulary and the engine trait:
/// wraps the engine's request into the facade type on the way in, and
/// unwraps the facade op into the engine op on the way out. Everything
/// stateful stays engine-owned.
pub(crate) struct EngineAuthPolicyAdapter<P> {
    policy: P,
}

impl<P> EngineAuthPolicyAdapter<P> {
    pub(crate) fn new(policy: P) -> Self {
        Self { policy }
    }
}

impl<P: AuthPolicy> nmp_runtime::AuthPolicy for EngineAuthPolicyAdapter<P> {
    fn evaluate(&self, request: nmp_runtime::AuthPolicyRequest) -> nmp_runtime::AuthPolicyOp {
        self.policy
            .evaluate(AuthPolicyRequest::from_engine(request))
            .into_engine()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #853: runtime policy evaluation uses these same types. If a second
    /// decision/error family returns, this assignment stops compiling.
    #[test]
    fn runtime_uses_the_same_auth_decision_and_error_types() {
        fn same_type<T>(_: T, _: T) {}
        let runtime_result = match AuthPolicyOp::allow().into_engine() {
            nmp_runtime::AuthPolicyOp::Ready(result) => result,
            nmp_runtime::AuthPolicyOp::Pending(_) => {
                panic!("ready constructor must produce a ready runtime op")
            }
        };
        same_type::<AuthPolicyResult>(Ok(AuthPolicyDecision::Allow), runtime_result);
    }

    /// The facade sender and the engine op share ONE channel — the newtype
    /// delegates rather than owning a second slot. Dropping the ENGINE op
    /// (the engine's terminal-cancel door) is what makes the FACADE
    /// sender's resolve report `ReceiverDropped`; a second facade resolve
    /// after a delivered first reports `AlreadyResolved`.
    #[test]
    fn facade_pending_sender_shares_the_engine_operations_channel() {
        // Engine cancels first: the facade sender observes it.
        let (sender, op) = AuthPolicyOp::pending_channel();
        drop(op.into_engine());
        assert!(matches!(
            sender.resolve(Ok(AuthPolicyDecision::Allow)),
            Err(AuthPolicyResolveError::ReceiverDropped(Ok(
                AuthPolicyDecision::Allow
            )))
        ));

        // App answers first: exactly one resolve lands.
        let (sender, _op) = AuthPolicyOp::pending_channel();
        sender
            .resolve(Ok(AuthPolicyDecision::Deny {
                reason: "prompt refused".to_string(),
            }))
            .expect("first resolve must deliver");
        assert!(matches!(
            sender.resolve(Ok(AuthPolicyDecision::Allow)),
            Err(AuthPolicyResolveError::AlreadyResolved(Ok(
                AuthPolicyDecision::Allow
            )))
        ));
    }

    /// The engine-owned Drop/terminal-cancel path invokes the app's cancel
    /// hook exactly once for an unresolved pending op — inherited by
    /// delegation, not re-implemented here.
    #[test]
    fn dropping_an_unresolved_pending_op_fires_the_cancel_hook_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let cancellations = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&cancellations);
        let (_sender, op) = AuthPolicyOp::pending_channel_with_cancel(move || {
            observed.fetch_add(1, Ordering::SeqCst);
        });
        drop(op.into_engine());
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
    }

    /// The ready constructors delegate to the runtime constructors and carry
    /// the exact shared result.
    #[test]
    fn ready_constructors_delegate_with_exact_results() {
        for (op, expected) in [
            (AuthPolicyOp::allow(), Ok(AuthPolicyDecision::Allow)),
            (
                AuthPolicyOp::deny("nope"),
                Ok(AuthPolicyDecision::Deny {
                    reason: "nope".to_string(),
                }),
            ),
            (
                AuthPolicyOp::ready(Err(AuthPolicyError::Technical {
                    reason: "hsm offline".to_string(),
                })),
                Err(AuthPolicyError::Technical {
                    reason: "hsm offline".to_string(),
                }),
            ),
        ] {
            match op.into_engine() {
                nmp_runtime::AuthPolicyOp::Ready(result) => assert_eq!(result, expected),
                nmp_runtime::AuthPolicyOp::Pending(_) => {
                    panic!("ready constructor must produce a ready engine op")
                }
            }
        }
    }
}
