//! Runtime ownership for reducer-owned NIP-42 operations.
//!
//! The reducer owns AUTH truth. This module owns only finite capability
//! registries, exact operation cancellation, and transport execution. Every
//! policy/signer task is bound to one checked capability instance before any
//! callback starts, and at most one such task is live for an exact relay
//! session.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use nmp_signer::{SignerError, SignerOp};
use nmp_transport::{
    EphemeralOperation, EphemeralSendOutcome, EphemeralSendStart, Pool, WireFrame,
};
use nostr::{ClientMessage, JsonUtil, PublicKey, RelayUrl};

use nmp_engine::core::{
    AuthCapability, AuthCapabilityInstance, AuthEffect, AuthEpoch, AuthOpToken, AuthPolicyOutcome,
    AuthSendCompletion, AuthSendOutcome, AuthSignerOutcome, EngineMsg,
};

use super::Cmd;
use crate::identity_sessions::{decode_signed_event, encode_unsigned_event, SignerRegistry};

/// App policy's closed semantic answer for one exact AUTH request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPolicyDecision {
    /// Authenticate this exact session: the reducer freezes and signs the
    /// canonical kind:22242 template for exactly this challenge.
    Allow,
    /// Refuse to authenticate; the session's protected work stays parked
    /// as `AuthDenied` evidence.
    Deny { reason: String },
}

/// Technical policy execution failures, separate from an explicit denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPolicyError {
    /// The policy could not run at all.
    Unavailable,
    /// The policy ran but failed for a technical reason.
    Technical { reason: String },
}

impl std::fmt::Display for AuthPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("AUTH policy unavailable"),
            Self::Technical { reason } => write!(f, "AUTH policy failed: {reason}"),
        }
    }
}

impl std::error::Error for AuthPolicyError {}

/// One policy answer: the app's semantic decision, or a technical failure.
pub type AuthPolicyResult = Result<AuthPolicyDecision, AuthPolicyError>;

/// Immutable input to one app-owned NIP-42 authorization decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthPolicyRequest {
    expected_pubkey: PublicKey,
    relay: RelayUrl,
    challenge: String,
    transport_generation: u64,
    epoch_sequence: u64,
}

impl AuthPolicyRequest {
    #[must_use]
    pub fn expected_pubkey(&self) -> PublicKey {
        self.expected_pubkey
    }

    #[must_use]
    pub fn relay(&self) -> &RelayUrl {
        &self.relay
    }

    #[must_use]
    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    #[must_use]
    pub fn transport_generation(&self) -> u64 {
        self.transport_generation
    }

    #[must_use]
    pub fn epoch_sequence(&self) -> u64 {
        self.epoch_sequence
    }
}

type PolicyResult = Result<AuthPolicyDecision, AuthPolicyError>;
type PendingPolicyCancel = Box<dyn FnOnce() + Send + 'static>;

// #704: the AUTH policy completion door is a hand-rolled waker-aware one-shot
// (mirroring `nmp-signer`'s `PendingSignerOp`), replacing the old
// `crossbeam bounded(1)` + `recv_or_cancel` handshake. The engine `.await`s it
// on the shared runtime, holding no OS thread while the app policy is pending;
// cancellation is bound in via a `PolicyCanceller`, and dropping an unresolved
// op (or its awaiting future) runs the adapter cancel hook exactly once.
enum PolicyDoorLifecycle {
    Open,
    /// A result was claimed; `None` means the consumer already took it.
    Resolved(Option<PolicyResult>),
    /// Cancellation won before a sender claimed the result.
    CancelledUnresolved,
    /// Cancellation is terminal. `Some` preserves a value that won first;
    /// `None` records that a later sender claim was truthfully refused.
    CancelledResolved(Option<PolicyResult>),
    ReceiverGoneUnresolved,
    ReceiverGoneResolved,
}

struct PolicyDoorState {
    lifecycle: PolicyDoorLifecycle,
    waker: Option<Waker>,
}

struct PolicyDoor {
    state: Mutex<PolicyDoorState>,
    senders: AtomicUsize,
}

impl PolicyDoor {
    fn take_waker(state: &mut PolicyDoorState) -> Option<Waker> {
        state.waker.take()
    }

    fn cancel(state: &mut PolicyDoorState) {
        let previous = std::mem::replace(&mut state.lifecycle, PolicyDoorLifecycle::Open);
        state.lifecycle = match previous {
            PolicyDoorLifecycle::Open => PolicyDoorLifecycle::CancelledUnresolved,
            PolicyDoorLifecycle::Resolved(value) => PolicyDoorLifecycle::CancelledResolved(value),
            PolicyDoorLifecycle::CancelledUnresolved => PolicyDoorLifecycle::CancelledUnresolved,
            PolicyDoorLifecycle::CancelledResolved(value) => {
                PolicyDoorLifecycle::CancelledResolved(value)
            }
            PolicyDoorLifecycle::ReceiverGoneUnresolved => {
                PolicyDoorLifecycle::ReceiverGoneUnresolved
            }
            PolicyDoorLifecycle::ReceiverGoneResolved => PolicyDoorLifecycle::ReceiverGoneResolved,
        };
    }

    fn mark_receiver_gone(state: &mut PolicyDoorState) {
        let previous = std::mem::replace(&mut state.lifecycle, PolicyDoorLifecycle::Open);
        state.lifecycle = match previous {
            PolicyDoorLifecycle::Open
            | PolicyDoorLifecycle::CancelledUnresolved
            | PolicyDoorLifecycle::ReceiverGoneUnresolved => {
                PolicyDoorLifecycle::ReceiverGoneUnresolved
            }
            PolicyDoorLifecycle::Resolved(_)
            | PolicyDoorLifecycle::CancelledResolved(_)
            | PolicyDoorLifecycle::ReceiverGoneResolved => {
                PolicyDoorLifecycle::ReceiverGoneResolved
            }
        };
    }
}

/// One-shot completion door for a pending [`AuthPolicyOp`].
pub struct AuthPolicyPendingSender {
    door: Arc<PolicyDoor>,
}

impl Clone for AuthPolicyPendingSender {
    fn clone(&self) -> Self {
        self.door.senders.fetch_add(1, Ordering::AcqRel);
        Self {
            door: Arc::clone(&self.door),
        }
    }
}

impl Drop for AuthPolicyPendingSender {
    fn drop(&mut self) {
        if self.door.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            let waker = {
                let mut state = self
                    .door
                    .state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                match state.lifecycle {
                    PolicyDoorLifecycle::Resolved(_)
                    | PolicyDoorLifecycle::CancelledResolved(_)
                    | PolicyDoorLifecycle::ReceiverGoneResolved => None,
                    PolicyDoorLifecycle::Open
                    | PolicyDoorLifecycle::CancelledUnresolved
                    | PolicyDoorLifecycle::ReceiverGoneUnresolved => {
                        PolicyDoor::take_waker(&mut state)
                    }
                }
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}

impl AuthPolicyPendingSender {
    pub fn resolve(&self, result: PolicyResult) -> Result<(), AuthPolicyResolveError> {
        let waker = {
            let mut state = self
                .door
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match state.lifecycle {
                PolicyDoorLifecycle::Open => {
                    state.lifecycle = PolicyDoorLifecycle::Resolved(Some(result));
                }
                PolicyDoorLifecycle::CancelledUnresolved => {
                    state.lifecycle = PolicyDoorLifecycle::CancelledResolved(None);
                    return Err(AuthPolicyResolveError::ReceiverDropped(result));
                }
                PolicyDoorLifecycle::ReceiverGoneUnresolved => {
                    state.lifecycle = PolicyDoorLifecycle::ReceiverGoneResolved;
                    return Err(AuthPolicyResolveError::ReceiverDropped(result));
                }
                PolicyDoorLifecycle::Resolved(_)
                | PolicyDoorLifecycle::CancelledResolved(_)
                | PolicyDoorLifecycle::ReceiverGoneResolved => {
                    return Err(AuthPolicyResolveError::AlreadyResolved(result));
                }
            }
            PolicyDoor::take_waker(&mut state)
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum AuthPolicyResolveError {
    AlreadyResolved(PolicyResult),
    ReceiverDropped(PolicyResult),
}

/// Cancels one [`PendingAuthPolicyOp`] (#704): wakes its awaiting future to a
/// "cancelled" (no-decision) end. Bound into the door itself.
#[derive(Clone)]
struct PolicyCanceller {
    door: Arc<PolicyDoor>,
}

impl PolicyCanceller {
    fn cancel(&self) {
        let waker = {
            let mut state = self
                .door
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            PolicyDoor::cancel(&mut state);
            PolicyDoor::take_waker(&mut state)
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

enum PendingPolicyLifecycle {
    Pending(Option<PendingPolicyCancel>),
    Finished,
}

#[derive(Clone, Copy)]
enum PendingPolicyFinish {
    CancelAdapter,
    SuppressCancel,
}

pub struct PendingAuthPolicyOp {
    door: Arc<PolicyDoor>,
    lifecycle: PendingPolicyLifecycle,
}

impl PendingAuthPolicyOp {
    fn canceller(&self) -> PolicyCanceller {
        PolicyCanceller {
            door: Arc::clone(&self.door),
        }
    }

    /// Drain a terminal outcome, or `None` while still open. The awaited value
    /// is `Some(result)` for a resolved decision or a disconnect (mapped to
    /// `Unavailable`), and `None` for a cancellation with no queued decision.
    /// A queued value is consumed before a cancellation is honored.
    fn take_terminal(state: &mut PolicyDoorState, senders: usize) -> Option<Option<PolicyResult>> {
        match &mut state.lifecycle {
            PolicyDoorLifecycle::Resolved(value) => Some(Some(
                value.take().unwrap_or(Err(AuthPolicyError::Unavailable)),
            )),
            PolicyDoorLifecycle::CancelledResolved(value) => match value.take() {
                Some(value) => Some(Some(value)),
                None => Some(None),
            },
            PolicyDoorLifecycle::CancelledUnresolved => Some(None),
            PolicyDoorLifecycle::Open if senders == 0 => {
                Some(Some(Err(AuthPolicyError::Unavailable)))
            }
            PolicyDoorLifecycle::ReceiverGoneUnresolved
            | PolicyDoorLifecycle::ReceiverGoneResolved => {
                Some(Some(Err(AuthPolicyError::Unavailable)))
            }
            PolicyDoorLifecycle::Open => None,
        }
    }

    fn finish(&mut self, disposition: PendingPolicyFinish) {
        let previous = std::mem::replace(&mut self.lifecycle, PendingPolicyLifecycle::Finished);
        let PendingPolicyLifecycle::Pending(cancel) = previous else {
            return;
        };
        if matches!(disposition, PendingPolicyFinish::CancelAdapter) {
            if let Some(cancel) = cancel {
                cancel();
            }
        }
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<PolicyResult>> {
        let mut state = self
            .door
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let senders = self.door.senders.load(Ordering::Acquire);
        if let Some(outcome) = Self::take_terminal(&mut state, senders) {
            // `None` is the cancelled-with-no-decision end — the only path that
            // runs the adapter cancel hook. A queued value (even under a racing
            // cancel) or a disconnect delivers a result and suppresses it.
            let disposition = match &outcome {
                None => PendingPolicyFinish::CancelAdapter,
                Some(_) => PendingPolicyFinish::SuppressCancel,
            };
            state.waker = None;
            drop(state);
            self.finish(disposition);
            return Poll::Ready(outcome);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Future for PendingAuthPolicyOp {
    type Output = Option<PolicyResult>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_recv(cx)
    }
}

impl Drop for PendingAuthPolicyOp {
    fn drop(&mut self) {
        // A future that already drained a terminal outcome (poll_recv →
        // finish) has run its cancel/complete linearization already.
        if matches!(self.lifecycle, PendingPolicyLifecycle::Finished) {
            return;
        }
        // Terminal-cancel door (an epoch/capability signal, or a dropped
        // awaiting future). This must reproduce the pre-#704 crossbeam
        // handshake: a resolver that already owns completion is still allowed
        // to deliver before the receiver is released, and a value already
        // committed to the door wins over cancellation (suppressing the
        // adapter cancel hook). Marking `receiver_gone` up front would defeat
        // an in-flight `resolve()` — so it is set only after the handshake.
        {
            let mut state = self
                .door
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let resolution_won = match &mut state.lifecycle {
                PolicyDoorLifecycle::Resolved(value) => {
                    let _ = value.take();
                    true
                }
                PolicyDoorLifecycle::CancelledResolved(value) if value.is_some() => {
                    let _ = value.take();
                    true
                }
                PolicyDoorLifecycle::Open
                | PolicyDoorLifecycle::CancelledUnresolved
                | PolicyDoorLifecycle::CancelledResolved(_)
                | PolicyDoorLifecycle::ReceiverGoneUnresolved
                | PolicyDoorLifecycle::ReceiverGoneResolved => false,
            };
            if resolution_won {
                PolicyDoor::mark_receiver_gone(&mut state);
                drop(state);
                self.finish(PendingPolicyFinish::SuppressCancel);
                return;
            }
        }
        // No committed resolution yet. Run the adapter cancel hook WITHOUT
        // holding the door lock and WITHOUT yet marking the receiver gone, so a
        // resolver racing this handshake still resolves successfully; the hook
        // blocks until that resolver finishes (or its sender disconnects).
        let previous = std::mem::replace(&mut self.lifecycle, PendingPolicyLifecycle::Finished);
        if let PendingPolicyLifecycle::Pending(Some(cancel)) = previous {
            cancel();
        }
        // The handshake is over: consume any value the racing resolver
        // delivered and release the receiver so a later resolve is refused.
        let mut state = self
            .door
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        PolicyDoor::mark_receiver_gone(&mut state);
    }
}

/// Nonblocking ready-or-pending policy operation.
pub enum AuthPolicyOp {
    Ready(PolicyResult),
    Pending(PendingAuthPolicyOp),
}

impl AuthPolicyOp {
    #[must_use]
    pub fn ready(result: PolicyResult) -> Self {
        Self::Ready(result)
    }

    #[must_use]
    pub fn allow() -> Self {
        Self::Ready(Ok(AuthPolicyDecision::Allow))
    }

    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Ready(Ok(AuthPolicyDecision::Deny {
            reason: reason.into(),
        }))
    }

    #[must_use]
    pub fn pending_channel() -> (AuthPolicyPendingSender, Self) {
        Self::pending_channel_from_cancel(None)
    }

    #[must_use]
    pub fn pending_channel_with_cancel(
        cancel: impl FnOnce() + Send + 'static,
    ) -> (AuthPolicyPendingSender, Self) {
        Self::pending_channel_from_cancel(Some(Box::new(cancel)))
    }

    fn pending_channel_from_cancel(
        cancel: Option<PendingPolicyCancel>,
    ) -> (AuthPolicyPendingSender, Self) {
        let door = Arc::new(PolicyDoor {
            state: Mutex::new(PolicyDoorState {
                lifecycle: PolicyDoorLifecycle::Open,
                waker: None,
            }),
            senders: AtomicUsize::new(1),
        });
        (
            AuthPolicyPendingSender {
                door: Arc::clone(&door),
            },
            Self::Pending(PendingAuthPolicyOp {
                door,
                lifecycle: PendingPolicyLifecycle::Pending(cancel),
            }),
        )
    }
}

/// App-owned NIP-42 authorization policy. `evaluate` must return a
/// ready-or-pending operation without blocking; the engine-owned async runtime
/// awaits it and owns pending cancellation.
pub trait AuthPolicy: Send {
    fn evaluate(&self, request: AuthPolicyRequest) -> AuthPolicyOp;
}

type SharedPolicy = Arc<Mutex<Box<dyn AuthPolicy>>>;

struct RegisteredPolicy {
    identity: Arc<()>,
    instance: AuthCapabilityInstance,
    policy: SharedPolicy,
}

#[derive(Default)]
pub(super) struct AuthPolicyRegistry {
    policies: HashMap<PublicKey, RegisteredPolicy>,
}

impl AuthPolicyRegistry {
    pub(super) fn contains(&self, expected_pubkey: PublicKey) -> bool {
        self.policies.contains_key(&expected_pubkey)
    }

    pub(super) fn len(&self) -> usize {
        self.policies.len()
    }

    pub(super) fn add(
        &mut self,
        expected_pubkey: PublicKey,
        instance: AuthCapabilityInstance,
        policy: Box<dyn AuthPolicy>,
    ) -> (AuthPolicyRegistration, Option<AuthCapabilityInstance>) {
        let identity = Arc::new(());
        let replaced = self
            .policies
            .insert(
                expected_pubkey,
                RegisteredPolicy {
                    identity: Arc::clone(&identity),
                    instance,
                    policy: Arc::new(Mutex::new(policy)),
                },
            )
            .map(|old| old.instance);
        (
            AuthPolicyRegistration {
                expected_pubkey,
                identity,
                instance,
            },
            replaced,
        )
    }

    pub(super) fn remove(
        &mut self,
        registration: &AuthPolicyRegistration,
    ) -> Option<AuthCapabilityInstance> {
        let is_current = self
            .policies
            .get(&registration.expected_pubkey)
            .is_some_and(|current| {
                current.instance == registration.instance
                    && Arc::ptr_eq(&current.identity, &registration.identity)
            });
        if !is_current {
            return None;
        }
        self.policies
            .remove(&registration.expected_pubkey)
            .map(|removed| removed.instance)
    }

    fn snapshot(
        &self,
        expected_pubkey: PublicKey,
    ) -> Option<(AuthCapabilityInstance, SharedPolicy)> {
        self.policies
            .get(&expected_pubkey)
            .map(|entry| (entry.instance, Arc::clone(&entry.policy)))
    }
}

/// Opaque ownership proof for one exact policy installation.
#[derive(Clone)]
pub struct AuthPolicyRegistration {
    expected_pubkey: PublicKey,
    identity: Arc<()>,
    instance: AuthCapabilityInstance,
}

impl AuthPolicyRegistration {
    #[must_use]
    pub fn expected_pubkey(&self) -> PublicKey {
        self.expected_pubkey
    }
}

impl std::fmt::Debug for AuthPolicyRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthPolicyRegistration")
            .field("expected_pubkey", &self.expected_pubkey)
            .finish_non_exhaustive()
    }
}

impl PartialEq for AuthPolicyRegistration {
    fn eq(&self, other: &Self) -> bool {
        self.expected_pubkey == other.expected_pubkey
            && self.instance == other.instance
            && Arc::ptr_eq(&self.identity, &other.identity)
    }
}

impl Eq for AuthPolicyRegistration {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddAuthPolicyError {
    CapabilityInstanceExhausted,
    RegistryFull { limit: usize },
    EngineShuttingDown,
}

impl std::fmt::Display for AddAuthPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapabilityInstanceExhausted => {
                f.write_str("AUTH capability instance space exhausted")
            }
            Self::RegistryFull { limit } => {
                write!(f, "AUTH capability registry is full at {limit} entries")
            }
            Self::EngineShuttingDown => f.write_str("engine is shutting down"),
        }
    }
}

impl std::error::Error for AddAuthPolicyError {}

pub(super) struct AuthCapabilityInstances {
    next: Option<u64>,
}

impl Default for AuthCapabilityInstances {
    fn default() -> Self {
        Self { next: Some(1) }
    }
}

impl AuthCapabilityInstances {
    pub(super) fn mint(&mut self) -> Option<AuthCapabilityInstance> {
        let issued = self.next?;
        self.next = issued.checked_add(1);
        Some(AuthCapabilityInstance(issued))
    }
}

#[repr(u8)]
#[derive(Clone, Copy)]
enum AuthTaskState {
    Open,
    Cancelled,
    Completed,
}

/// #704: the AUTH task's cancellation is bound to whatever in-flight pending op
/// the async operation is currently awaiting (policy OR signer — an operation
/// awaits at most one at a time). The operation `arm`s that op's canceller into
/// this terminal; `cancel()` fires it once, waking the awaiting future to a
/// cancelled end. No crossbeam channels remain.
struct AuthTaskTerminal {
    state: AtomicU8,
    canceller: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl AuthTaskTerminal {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(AuthTaskState::Open as u8),
            canceller: Mutex::new(None),
        })
    }

    /// Install the canceller for the pending op the operation is about to
    /// await. If the terminal is already cancelled, fire it immediately.
    fn arm(&self, canceller: Box<dyn Fn() + Send>) {
        let guard = self
            .canceller
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if self.state.load(Ordering::Acquire) == AuthTaskState::Cancelled as u8 {
            drop(guard);
            canceller();
        } else {
            let mut guard = guard;
            *guard = Some(canceller);
        }
    }

    fn is_open(&self) -> bool {
        self.state.load(Ordering::Acquire) == AuthTaskState::Open as u8
    }

    fn cancel(&self) -> bool {
        let mut guard = self
            .canceller
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if self
            .state
            .compare_exchange(
                AuthTaskState::Open as u8,
                AuthTaskState::Cancelled as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        if let Some(canceller) = guard.take() {
            canceller();
        }
        true
    }

    fn complete(&self) -> bool {
        self.state
            .compare_exchange(
                AuthTaskState::Open as u8,
                AuthTaskState::Completed as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

struct ActiveAuthTask {
    token: AuthOpToken,
    capability: AuthCapability,
    instance: AuthCapabilityInstance,
    terminal: Arc<AuthTaskTerminal>,
}

/// #704: the AUTH operation is an async future — it awaits the policy/signer
/// completion door on the shared runtime, holding no OS thread. It arms its
/// in-flight pending op's canceller into the terminal so cancellation reaches
/// whatever it is currently awaiting.
type AuthTaskOperation = Box<
    dyn FnOnce(
            Arc<AuthTaskTerminal>,
        ) -> Pin<Box<dyn Future<Output = Option<AuthTaskOutcome>> + Send>>
        + Send
        + 'static,
>;

pub(super) struct PendingAuthTask {
    token: AuthOpToken,
    capability: AuthCapability,
    instance: AuthCapabilityInstance,
    operation: AuthTaskOperation,
}

/// #704: the destructor-free release edge that replaces the executor
/// `ReleaseId`. The async AUTH task posts this once its work is done (or its
/// future is dropped); the reducer's [`AuthTaskRegistry::released`] then
/// removes the exact active task and launches any pending replacement for the
/// same session, exactly as the old reaper-driven release did.
pub(super) struct AuthTaskReleaseToken {
    session: nmp_grammar::RelaySessionKey,
    terminal: Arc<AuthTaskTerminal>,
}

#[derive(Default)]
pub(super) struct AuthTaskRegistry {
    active: HashMap<nmp_grammar::RelaySessionKey, ActiveAuthTask>,
    pending: HashMap<nmp_grammar::RelaySessionKey, PendingAuthTask>,
}

impl AuthTaskRegistry {
    fn schedule(&mut self, task: PendingAuthTask) -> Option<PendingAuthTask> {
        let session = task.token.epoch.session.clone();
        if let Some(active) = self.active.get(&session) {
            active.terminal.cancel();
            self.pending.insert(session, task);
            None
        } else {
            Some(task)
        }
    }

    fn started(&mut self, task: ActiveAuthTask) {
        self.active.insert(task.token.epoch.session.clone(), task);
    }

    pub(super) fn cancel_epoch(&mut self, epoch: &AuthEpoch) {
        let should_cancel = self
            .active
            .get(&epoch.session)
            .is_some_and(|task| task.token.epoch == *epoch);
        if should_cancel {
            if let Some(task) = self.active.get(&epoch.session) {
                task.terminal.cancel();
            }
        }
        self.pending.retain(|_, task| task.token.epoch != *epoch);
    }

    pub(super) fn cancel_capability(
        &mut self,
        pubkey: PublicKey,
        capability: AuthCapability,
        instance: AuthCapabilityInstance,
    ) {
        let sessions: Vec<_> = self
            .active
            .iter()
            .filter_map(|(session, task)| {
                let same_pubkey = matches!(
                    task.token.epoch.session.authenticate_as,
                    Some(current) if current == pubkey
                );
                (same_pubkey && task.capability == capability && task.instance == instance)
                    .then(|| session.clone())
            })
            .collect();
        for session in sessions {
            if let Some(task) = self.active.get(&session) {
                task.terminal.cancel();
            }
        }
        self.pending.retain(|_, task| {
            let same_pubkey = matches!(
                task.token.epoch.session.authenticate_as,
                Some(current) if current == pubkey
            );
            !(same_pubkey && task.capability == capability && task.instance == instance)
        });
    }

    pub(super) fn finish(&mut self, completion: AuthTaskCompletion) -> Option<EngineMsg> {
        let session = &completion.token.epoch.session;
        let exact = self.active.get(session).is_some_and(|task| {
            task.token == completion.token
                && task.capability == completion.capability
                && task.instance == completion.instance
                && Arc::ptr_eq(&task.terminal, &completion.terminal)
        });
        if !exact {
            return None;
        }
        Some(
            completion
                .outcome
                .into_engine_msg(completion.token, completion.instance),
        )
    }

    pub(super) fn released(&mut self, token: AuthTaskReleaseToken) -> Option<PendingAuthTask> {
        let exact = self
            .active
            .get(&token.session)
            .is_some_and(|task| Arc::ptr_eq(&task.terminal, &token.terminal));
        if !exact {
            return None;
        }
        self.active.remove(&token.session);
        self.pending.remove(&token.session)
    }

    pub(super) fn shutdown(&mut self) {
        for task in self.active.values() {
            task.terminal.cancel();
        }
        self.pending.clear();
    }

    /// No AUTH task of this registry can still run FOREIGN capability code.
    /// Read only by the shutdown drain (`super::foreign_work_drained`).
    pub(super) fn is_drained(&self) -> bool {
        self.active.is_empty() && self.pending.is_empty()
    }

}

pub(super) struct AuthTaskCompletion {
    token: AuthOpToken,
    capability: AuthCapability,
    instance: AuthCapabilityInstance,
    terminal: Arc<AuthTaskTerminal>,
    outcome: AuthTaskOutcome,
}

enum AuthTaskOutcome {
    Policy(AuthPolicyOutcome),
    Signer(AuthSignerOutcome),
}

impl AuthTaskOutcome {
    fn into_engine_msg(self, token: AuthOpToken, instance: AuthCapabilityInstance) -> EngineMsg {
        match self {
            Self::Policy(outcome) => EngineMsg::AuthPolicyCompleted(token, Some(instance), outcome),
            Self::Signer(outcome) => EngineMsg::AuthSignerCompleted(token, Some(instance), outcome),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch(
    effect: AuthEffect,
    pool: &Pool,
    signers: &SignerRegistry,
    policies: &AuthPolicyRegistry,
    tasks: &mut AuthTaskRegistry,
    runtime: &tokio::runtime::Handle,
    inbox: &Sender<Cmd>,
    bind: &mut impl FnMut(AuthOpToken, AuthCapability, AuthCapabilityInstance),
) {
    match effect {
        AuthEffect::Cancel(epoch) => tasks.cancel_epoch(&epoch),
        AuthEffect::RequestPolicy {
            token,
            expected_pubkey,
            challenge,
        } => {
            let Some((instance, policy)) = policies.snapshot(expected_pubkey) else {
                let _ = inbox.send(Cmd::Engine(EngineMsg::AuthPolicyCompleted(
                    token,
                    None,
                    AuthPolicyOutcome::Unavailable,
                )));
                return;
            };
            let request = AuthPolicyRequest {
                expected_pubkey,
                relay: token.epoch.session.relay.clone(),
                challenge,
                transport_generation: token.epoch.handle.generation,
                epoch_sequence: token.epoch.sequence,
            };
            start_auth_task(
                token,
                AuthCapability::Policy,
                instance,
                tasks,
                runtime,
                inbox,
                bind,
                move |terminal| {
                    Box::pin(async move {
                        let op = policy
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .evaluate(request);
                        let result: Option<PolicyResult> = match op {
                            AuthPolicyOp::Ready(result) => Some(result),
                            AuthPolicyOp::Pending(pending) => {
                                let canceller = pending.canceller();
                                terminal.arm(Box::new(move || canceller.cancel()));
                                pending.await
                            }
                        };
                        if !terminal.is_open() {
                            return None;
                        }
                        Some(AuthTaskOutcome::Policy(match result? {
                            Ok(AuthPolicyDecision::Allow) => AuthPolicyOutcome::Allow,
                            Ok(AuthPolicyDecision::Deny { reason }) => {
                                AuthPolicyOutcome::Deny { reason }
                            }
                            Err(AuthPolicyError::Unavailable) => AuthPolicyOutcome::Unavailable,
                            Err(AuthPolicyError::Technical { reason }) => {
                                AuthPolicyOutcome::Error { reason }
                            }
                        }))
                    })
                },
            );
        }
        AuthEffect::RequestSignature { token, unsigned } => {
            let Some((instance, signer)) = signers.auth_snapshot(unsigned.pubkey) else {
                let _ = inbox.send(Cmd::Engine(EngineMsg::AuthSignerCompleted(
                    token,
                    None,
                    AuthSignerOutcome::Unavailable,
                )));
                return;
            };
            start_auth_task(
                token,
                AuthCapability::Signer,
                instance,
                tasks,
                runtime,
                inbox,
                bind,
                move |terminal| {
                    Box::pin(async move {
                        let op = signer.sign(encode_unsigned_event(&unsigned));
                        let result: Option<Result<nostr::Event, SignerError>> = match op {
                            SignerOp::Ready(result) => Some(result.and_then(decode_signed_event)),
                            SignerOp::Pending(pending) => {
                                let canceller = pending.canceller();
                                terminal.arm(Box::new(move || canceller.cancel()));
                                Some(pending.await.and_then(decode_signed_event))
                            }
                        };
                        if !terminal.is_open() {
                            return None;
                        }
                        Some(AuthTaskOutcome::Signer(match result? {
                            Ok(event) => AuthSignerOutcome::Signed(event),
                            Err(SignerError::Rejected(reason)) => {
                                AuthSignerOutcome::Rejected { reason }
                            }
                            Err(SignerError::InvalidResponse(reason)) => {
                                AuthSignerOutcome::Error { reason }
                            }
                            Err(SignerError::Unavailable) => AuthSignerOutcome::Unavailable,
                            Err(error @ (SignerError::Timeout | SignerError::Disconnected)) => {
                                AuthSignerOutcome::Error {
                                    reason: error.to_string(),
                                }
                            }
                        }))
                    })
                },
            );
        }
        AuthEffect::Send { token, event } => {
            // Issue #883: the pool takes an opaque operation token, never a
            // closure. An accepted operation resolves through the ordinary
            // `PoolEvent::EphemeralHandoff` path (see
            // `runtime::translate_pool_event`); only a synchronous refusal is
            // reported from here, and the two are mutually exclusive by
            // `EphemeralSendStart`'s construction.
            let start = pool.send_ephemeral_exact(
                &token.epoch.session,
                token.epoch.handle,
                EphemeralOperation(token.sequence),
                WireFrame::Text(ClientMessage::auth(*event).as_json()),
            );
            if let EphemeralSendStart::Resolved(outcome) = start {
                let _ = inbox.send(Cmd::Engine(EngineMsg::AuthSendCompleted(
                    AuthSendCompletion::for_operation(&token, auth_send_outcome(outcome)),
                )));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_auth_task(
    token: AuthOpToken,
    capability: AuthCapability,
    instance: AuthCapabilityInstance,
    tasks: &mut AuthTaskRegistry,
    runtime: &tokio::runtime::Handle,
    inbox: &Sender<Cmd>,
    bind: &mut impl FnMut(AuthOpToken, AuthCapability, AuthCapabilityInstance),
    operation: impl FnOnce(
            Arc<AuthTaskTerminal>,
        ) -> Pin<Box<dyn Future<Output = Option<AuthTaskOutcome>> + Send>>
        + Send
        + 'static,
) {
    bind(token.clone(), capability, instance);
    let pending = PendingAuthTask {
        token,
        capability,
        instance,
        operation: Box::new(operation),
    };
    if let Some(ready) = tasks.schedule(pending) {
        launch_auth_task(ready, tasks, runtime, inbox);
    }
}

/// #704: run one AUTH policy/signer operation as an async task on the engine
/// runtime. It reserves NO admission slot (there is none). When the task's work
/// is done — or its future is dropped (runtime shutdown / cancellation) — the
/// release drop guard posts [`Cmd::AuthTaskReleased`], which removes the exact
/// active task and launches any pending replacement for the same session; this
/// preserves the one-active-task-per-session serialization the executor
/// `ReleaseId` reaper used to drive.
pub(super) fn launch_auth_task(
    task: PendingAuthTask,
    tasks: &mut AuthTaskRegistry,
    runtime: &tokio::runtime::Handle,
    inbox: &Sender<Cmd>,
) {
    let PendingAuthTask {
        token,
        capability,
        instance,
        operation,
    } = task;
    let terminal = AuthTaskTerminal::new();
    tasks.started(ActiveAuthTask {
        token: token.clone(),
        capability,
        instance,
        terminal: Arc::clone(&terminal),
    });

    let completion_inbox = inbox.clone();
    let release = AuthReleaseGuard {
        inbox: inbox.clone(),
        token: Some(AuthTaskReleaseToken {
            session: token.epoch.session.clone(),
            terminal: Arc::clone(&terminal),
        }),
    };
    let op_terminal = Arc::clone(&terminal);
    runtime.spawn(async move {
        // Dropped whether the task completes normally or is aborted, so a
        // pending replacement always launches (the reaper-release invariant).
        let _release = release;
        let outcome = if op_terminal.is_open() {
            // Catch a panic from the foreign policy/signer call (which runs on
            // first poll) and map it to a typed Error outcome, matching the
            // executor's old catch_unwind. A panic on a tokio task otherwise
            // just terminates that task, leaving the AUTH op unresolved.
            match CatchUnwind::new(operation(Arc::clone(&op_terminal))).await {
                Ok(outcome) => outcome,
                Err(_) => Some(match capability {
                    AuthCapability::Policy => AuthTaskOutcome::Policy(AuthPolicyOutcome::Error {
                        reason: "AUTH policy panicked".to_string(),
                    }),
                    AuthCapability::Signer => AuthTaskOutcome::Signer(AuthSignerOutcome::Error {
                        reason: "AUTH signer panicked".to_string(),
                    }),
                }),
            }
        } else {
            None
        };
        if let Some(outcome) = outcome.filter(|_| op_terminal.complete()) {
            let completion = AuthTaskCompletion {
                token,
                capability,
                instance,
                terminal: op_terminal,
                outcome,
            };
            let _ = completion_inbox.send(Cmd::AuthTaskCompleted(completion));
        }
    });
}

/// Posts the destructor-free AUTH release edge when the async task finishes or
/// is dropped.
struct AuthReleaseGuard {
    inbox: Sender<Cmd>,
    token: Option<AuthTaskReleaseToken>,
}

impl Drop for AuthReleaseGuard {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            let _ = self.inbox.send(Cmd::AuthTaskReleased(token));
        }
    }
}

/// Minimal `catch_unwind` future adapter (avoids a `futures` dependency). A
/// panic while polling `inner` — including the synchronous foreign
/// policy/signer call on first poll — is captured instead of aborting the task.
struct CatchUnwind<F> {
    inner: F,
}

impl<F> CatchUnwind<F> {
    fn new(inner: F) -> Self {
        Self { inner }
    }
}

impl<F: Future> Future for CatchUnwind<F> {
    type Output = std::thread::Result<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: standard pin-projection; `inner` is never moved out.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(panic) => Poll::Ready(Err(panic)),
        }
    }
}

pub(super) fn auth_send_outcome(outcome: EphemeralSendOutcome) -> AuthSendOutcome {
    match outcome {
        EphemeralSendOutcome::Accepted => AuthSendOutcome::Accepted,
        EphemeralSendOutcome::Unavailable => AuthSendOutcome::Unavailable,
    }
}

