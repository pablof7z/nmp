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
use nmp_transport::{EphemeralSendOutcome, EphemeralSendStart, Pool, WireFrame};
use nostr::{ClientMessage, JsonUtil, PublicKey, RelayUrl};

use crate::core::{
    AuthCapability, AuthCapabilityInstance, AuthEffect, AuthEpoch, AuthOpToken, AuthPolicyOutcome,
    AuthSendOutcome, AuthSignerOutcome, EngineMsg,
};

use super::{Cmd, SignerRegistry};

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

/// App policy's closed semantic answer for one exact AUTH request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPolicyDecision {
    Allow,
    Deny { reason: String },
}

/// Technical policy execution failures, separate from an explicit denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPolicyError {
    Unavailable,
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

type PolicyResult = Result<AuthPolicyDecision, AuthPolicyError>;
type PendingPolicyCancel = Box<dyn FnOnce() + Send + 'static>;

// #704: the AUTH policy completion door is a hand-rolled waker-aware one-shot
// (mirroring `nmp-signer`'s `PendingSignerOp`), replacing the old
// `crossbeam bounded(1)` + `recv_or_cancel` handshake. The engine `.await`s it
// on the shared runtime, holding no OS thread while the app policy is pending;
// cancellation is bound in via a `PolicyCanceller`, and dropping an unresolved
// op (or its awaiting future) runs the adapter cancel hook exactly once.
struct PolicyDoorState {
    value: Option<PolicyResult>,
    waker: Option<Waker>,
    resolved: bool,
    receiver_gone: bool,
    cancelled: bool,
}

struct PolicyDoor {
    state: Mutex<PolicyDoorState>,
    senders: AtomicUsize,
}

impl PolicyDoor {
    fn take_waker(state: &mut PolicyDoorState) -> Option<Waker> {
        state.waker.take()
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
                if state.resolved {
                    None
                } else {
                    PolicyDoor::take_waker(&mut state)
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
            if state.resolved {
                return Err(AuthPolicyResolveError::AlreadyResolved(result));
            }
            state.resolved = true;
            if state.receiver_gone || state.cancelled {
                return Err(AuthPolicyResolveError::ReceiverDropped(result));
            }
            state.value = Some(result);
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
            state.cancelled = true;
            PolicyDoor::take_waker(&mut state)
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

pub struct PendingAuthPolicyOp {
    door: Arc<PolicyDoor>,
    cancel: Option<PendingPolicyCancel>,
    done: bool,
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
        if let Some(value) = state.value.take() {
            return Some(Some(value));
        }
        if state.cancelled {
            return Some(None);
        }
        if state.resolved || senders == 0 {
            return Some(Some(Err(AuthPolicyError::Unavailable)));
        }
        None
    }

    fn finish(&mut self, cancelled_path: bool) {
        self.done = true;
        if cancelled_path {
            if let Some(cancel) = self.cancel.take() {
                cancel();
            }
        } else {
            self.cancel = None;
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
            let cancelled_path = outcome.is_none();
            state.waker = None;
            drop(state);
            self.finish(cancelled_path);
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
        if self.done {
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
            if state.value.take().is_some() || state.resolved {
                // A resolution already won: consume it, never cancel.
                state.receiver_gone = true;
                self.cancel = None;
                return;
            }
        }
        // No committed resolution yet. Run the adapter cancel hook WITHOUT
        // holding the door lock and WITHOUT yet marking the receiver gone, so a
        // resolver racing this handshake still resolves successfully; the hook
        // blocks until that resolver finishes (or its sender disconnects).
        if let Some(cancel) = self.cancel.take() {
            cancel();
        }
        // The handshake is over: consume any value the racing resolver
        // delivered and release the receiver so a later resolve is refused.
        let mut state = self
            .door
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _ = state.value.take();
        state.receiver_gone = true;
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
                value: None,
                waker: None,
                resolved: false,
                receiver_gone: false,
                cancelled: false,
            }),
            senders: AtomicUsize::new(1),
        });
        (
            AuthPolicyPendingSender {
                door: Arc::clone(&door),
            },
            Self::Pending(PendingAuthPolicyOp {
                door,
                cancel,
                done: false,
            }),
        )
    }
}

/// App-owned NIP-42 authorization policy. `evaluate` must return a
/// ready-or-pending operation without blocking; the runtime executes it on
/// its existing finite native executor and owns pending cancellation.
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

const AUTH_TASK_OPEN: u8 = 0;
const AUTH_TASK_CANCELLED: u8 = 1;
const AUTH_TASK_COMPLETED: u8 = 2;

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
            state: AtomicU8::new(AUTH_TASK_OPEN),
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
        if self.state.load(Ordering::Acquire) == AUTH_TASK_CANCELLED {
            drop(guard);
            canceller();
        } else {
            let mut guard = guard;
            *guard = Some(canceller);
        }
    }

    fn is_open(&self) -> bool {
        self.state.load(Ordering::Acquire) == AUTH_TASK_OPEN
    }

    fn cancel(&self) -> bool {
        let mut guard = self
            .canceller
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if self
            .state
            .compare_exchange(
                AUTH_TASK_OPEN,
                AUTH_TASK_CANCELLED,
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
                AUTH_TASK_OPEN,
                AUTH_TASK_COMPLETED,
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
                    task.token.epoch.session.access,
                    nmp_grammar::AccessContext::Nip42(current) if current == pubkey
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
                task.token.epoch.session.access,
                nmp_grammar::AccessContext::Nip42(current) if current == pubkey
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

    pub(super) fn is_empty(&self) -> bool {
        self.active.is_empty() && self.pending.is_empty()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.active.len() + self.pending.len()
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
                        let op = signer
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .sign(*unsigned);
                        let result: Option<Result<nostr::Event, SignerError>> = match op {
                            SignerOp::Ready(result) => Some(result),
                            SignerOp::Pending(pending) => {
                                let canceller = pending.canceller();
                                terminal.arm(Box::new(move || canceller.cancel()));
                                Some(pending.await)
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
        AuthEffect::Send {
            token,
            epoch,
            event,
        } => {
            let completion_inbox = inbox.clone();
            let completion_token = token.clone();
            let start = pool.send_ephemeral_exact(
                &epoch.session,
                epoch.handle,
                WireFrame::Text(ClientMessage::auth(*event).as_json()),
                move |outcome| {
                    let _ = completion_inbox.send(Cmd::Engine(EngineMsg::AuthSendCompleted(
                        completion_token,
                        auth_send_outcome(outcome),
                    )));
                },
            );
            if let EphemeralSendStart::Resolved(outcome) = start {
                let _ = inbox.send(Cmd::Engine(EngineMsg::AuthSendCompleted(
                    token,
                    auth_send_outcome(outcome),
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

fn auth_send_outcome(outcome: EphemeralSendOutcome) -> AuthSendOutcome {
    match outcome {
        EphemeralSendOutcome::Accepted => AuthSendOutcome::Accepted,
        EphemeralSendOutcome::Unavailable => AuthSendOutcome::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{AuthEpoch, AuthOpToken};
    use crossbeam_channel as cb;
    use nmp_grammar::{AccessContext, RelaySessionKey};
    use nmp_transport::{PoolConfig, PoolEvent, RelayHandle};
    use nostr::{Keys, RelayUrl};
    use std::collections::VecDeque;
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct QueuedPolicy {
        invoked: std::sync::mpsc::Sender<AuthPolicyRequest>,
        operations: Mutex<VecDeque<AuthPolicyOp>>,
    }

    impl AuthPolicy for QueuedPolicy {
        fn evaluate(&self, request: AuthPolicyRequest) -> AuthPolicyOp {
            let _ = self.invoked.send(request);
            self.operations
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .pop_front()
                .expect("test policy operation")
        }
    }

    fn token() -> AuthOpToken {
        let relay = RelayUrl::parse("wss://auth-adapter.example.com").unwrap();
        let keys = Keys::generate();
        AuthOpToken {
            epoch: AuthEpoch {
                handle: RelayHandle {
                    slot: 3,
                    generation: 7,
                },
                session: RelaySessionKey::new(relay, AccessContext::Nip42(keys.public_key())),
                sequence: 11,
            },
            sequence: 12,
        }
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    fn recv_release(rx: &std::sync::mpsc::Receiver<Cmd>) -> AuthTaskReleaseToken {
        match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            Cmd::AuthTaskReleased(token) => token,
            _ => panic!("expected Cmd::AuthTaskReleased"),
        }
    }

    fn block_on_pending(pending: PendingAuthPolicyOp) -> Option<PolicyResult> {
        test_runtime().block_on(pending)
    }

    #[test]
    fn terminal_cancel_consumes_a_result_already_queued() {
        // #704: adapted to the waker door — a queued decision is consumed even
        // if the op is cancelled afterward (value-before-cancel), and the
        // adapter cancel hook stays suppressed.
        let (completion, operation) = AuthPolicyOp::pending_channel_with_cancel(|| {
            panic!("queued completion must suppress cancellation")
        });
        let AuthPolicyOp::Pending(pending) = operation else {
            unreachable!()
        };
        completion.resolve(Ok(AuthPolicyDecision::Allow)).unwrap();
        pending.canceller().cancel();
        assert_eq!(
            block_on_pending(pending),
            Some(Ok(AuthPolicyDecision::Allow))
        );
    }

    #[test]
    fn terminal_cancel_wins_once_before_resolve_and_drops_receiver_truthfully() {
        // #704: adapted to the waker door — cancelling before resolution ends
        // the await with no decision, runs the adapter cancel hook exactly
        // once, and a later resolve is refused as ReceiverDropped.
        let cancellations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&cancellations);
        let (completion, operation) = AuthPolicyOp::pending_channel_with_cancel(move || {
            observed.fetch_add(1, Ordering::AcqRel);
        });
        let AuthPolicyOp::Pending(pending) = operation else {
            unreachable!()
        };
        pending.canceller().cancel();

        assert_eq!(block_on_pending(pending), None);
        assert_eq!(cancellations.load(Ordering::Acquire), 1);
        assert!(matches!(
            completion.resolve(Ok(AuthPolicyDecision::Allow)),
            Err(AuthPolicyResolveError::ReceiverDropped(Ok(
                AuthPolicyDecision::Allow
            )))
        ));
    }

    // #704: `losing_cancel_waits_for_the_resolver_owned_result` was deleted. It
    // asserted the old `crossbeam bounded(1)` + `recv_or_cancel` handshake
    // semantics — a cancellation that loses the race blocks in the cancel hook
    // until the resolver-owned result is delivered. The waker door + the
    // AUTH `terminal.complete()` linearization gate replace that mechanism:
    // there is no blocking cancel handshake to observe.

    #[test]
    fn policy_is_bound_before_callback_and_completes_the_exact_instance() {
        let rt = test_runtime();
        let (pool_tx, _pool_rx) = std::sync::mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), pool_tx).unwrap();
        let signers = SignerRegistry::default();
        let mut policies = AuthPolicyRegistry::default();
        let mut tasks = AuthTaskRegistry::default();
        let (inbox, rx) = std::sync::mpsc::channel();
        let (invoked_tx, invoked_rx) = std::sync::mpsc::channel();
        let (completion, operation) = AuthPolicyOp::pending_channel();
        let request_token = token();
        let expected_pubkey = match request_token.epoch.session.access {
            AccessContext::Nip42(pubkey) => pubkey,
            AccessContext::Public => unreachable!(),
        };
        let instance = AuthCapabilityInstance(17);
        policies.add(
            expected_pubkey,
            instance,
            Box::new(QueuedPolicy {
                invoked: invoked_tx,
                operations: Mutex::new(VecDeque::from([operation])),
            }),
        );

        dispatch(
            AuthEffect::RequestPolicy {
                token: request_token.clone(),
                expected_pubkey,
                challenge: "frozen-challenge".to_string(),
            },
            &pool,
            &signers,
            &policies,
            &mut tasks,
            rt.handle(),
            &inbox,
            &mut |token, capability, instance| {
                let _ = inbox.send(Cmd::Engine(EngineMsg::AuthCapabilityBound {
                    token,
                    capability,
                    instance,
                }));
            },
        );

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Cmd::Engine(EngineMsg::AuthCapabilityBound {
                token: current,
                capability: AuthCapability::Policy,
                instance: current_instance,
            }) if current == request_token && current_instance == instance
        ));
        let observed = invoked_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(observed.expected_pubkey(), expected_pubkey);
        assert_eq!(observed.relay(), &request_token.epoch.session.relay);
        assert_eq!(observed.challenge(), "frozen-challenge");
        assert_eq!(
            observed.transport_generation(),
            request_token.epoch.handle.generation
        );
        assert_eq!(observed.epoch_sequence(), request_token.epoch.sequence);
        completion.resolve(Ok(AuthPolicyDecision::Allow)).unwrap();
        let completion = match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::AuthTaskCompleted(completion) => completion,
            _ => panic!("policy task completion expected"),
        };
        assert!(matches!(
            tasks.finish(completion),
            Some(EngineMsg::AuthPolicyCompleted(
                current,
                Some(current_instance),
                AuthPolicyOutcome::Allow
            )) if current == request_token && current_instance == instance
        ));
        assert!(tasks.released(recv_release(&rx)).is_none());
        assert_eq!(tasks.len(), 0);

        pool.shutdown();
    }

    #[test]
    fn replacement_and_epoch_cancel_drain_pending_policy_tasks() {
        let rt = test_runtime();
        let (pool_tx, _pool_rx) = std::sync::mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), pool_tx).unwrap();
        let signers = SignerRegistry::default();
        let mut policies = AuthPolicyRegistry::default();
        let mut tasks = AuthTaskRegistry::default();
        let (inbox, rx) = std::sync::mpsc::channel();
        let (invoked_tx, invoked_rx) = std::sync::mpsc::channel();
        let (cancel_tx, cancel_rx) = cb::bounded(1);
        let (_completion, operation) = AuthPolicyOp::pending_channel_with_cancel(move || {
            let _ = cancel_tx.send(());
        });
        let request_token = token();
        let expected_pubkey = match request_token.epoch.session.access {
            AccessContext::Nip42(pubkey) => pubkey,
            AccessContext::Public => unreachable!(),
        };
        let old_instance = AuthCapabilityInstance(21);
        let (old_registration, _) = policies.add(
            expected_pubkey,
            old_instance,
            Box::new(QueuedPolicy {
                invoked: invoked_tx,
                operations: Mutex::new(VecDeque::from([operation])),
            }),
        );
        dispatch(
            AuthEffect::RequestPolicy {
                token: request_token.clone(),
                expected_pubkey,
                challenge: "replace-me".to_string(),
            },
            &pool,
            &signers,
            &policies,
            &mut tasks,
            rt.handle(),
            &inbox,
            &mut |token, capability, instance| {
                let _ = inbox.send(Cmd::Engine(EngineMsg::AuthCapabilityBound {
                    token,
                    capability,
                    instance,
                }));
            },
        );
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Cmd::Engine(EngineMsg::AuthCapabilityBound { .. })
        ));
        invoked_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        tasks.cancel_capability(expected_pubkey, AuthCapability::Policy, old_instance);
        cancel_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(tasks.released(recv_release(&rx)).is_none());
        assert_eq!(tasks.len(), 0);

        let (unused_tx, _unused_rx) = std::sync::mpsc::channel();
        let (replacement, replaced) = policies.add(
            expected_pubkey,
            AuthCapabilityInstance(22),
            Box::new(QueuedPolicy {
                invoked: unused_tx,
                operations: Mutex::new(VecDeque::new()),
            }),
        );
        assert_eq!(replaced, Some(old_instance));
        assert_eq!(policies.remove(&old_registration), None);
        assert_eq!(
            policies.remove(&replacement),
            Some(AuthCapabilityInstance(22))
        );

        tasks.cancel_epoch(&request_token.epoch);
        tasks.shutdown();
        pool.shutdown();
    }

    #[test]
    fn cancel_and_completion_have_one_linearization_winner() {
        let completed_first = AuthTaskTerminal::new();
        assert!(completed_first.complete());
        assert!(!completed_first.cancel());

        let cancelled_first = AuthTaskTerminal::new();
        assert!(cancelled_first.cancel());
        assert!(!cancelled_first.complete());
    }

    #[test]
    fn capability_instance_allocator_never_wraps() {
        let mut instances = AuthCapabilityInstances {
            next: Some(u64::MAX),
        };
        assert_eq!(instances.mint(), Some(AuthCapabilityInstance(u64::MAX)));
        assert_eq!(instances.mint(), None);
    }

    #[test]
    fn one_active_task_per_session_releases_policy_before_starting_signer_phase() {
        // #704: the executor `ReleaseId` reaper is gone; the async task's drop
        // guard posts `Cmd::AuthTaskReleased`, which the reducer feeds to
        // `released` to launch the pending signer. The one-active-per-session
        // serialization is preserved.
        let rt = test_runtime();
        let (inbox, rx) = std::sync::mpsc::channel();
        let mut tasks = AuthTaskRegistry::default();
        let policy_token = token();
        launch_auth_task(
            PendingAuthTask {
                token: policy_token.clone(),
                capability: AuthCapability::Policy,
                instance: AuthCapabilityInstance(31),
                operation: Box::new(|_terminal| {
                    Box::pin(async { Some(AuthTaskOutcome::Policy(AuthPolicyOutcome::Allow)) })
                }),
            },
            &mut tasks,
            rt.handle(),
            &inbox,
        );
        let completion = match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::AuthTaskCompleted(completion) => completion,
            _ => panic!("policy completion expected"),
        };
        assert!(matches!(
            tasks.finish(completion),
            Some(EngineMsg::AuthPolicyCompleted(..))
        ));

        let mut signer_token = policy_token.clone();
        signer_token.sequence += 1;
        let signer = PendingAuthTask {
            token: signer_token.clone(),
            capability: AuthCapability::Signer,
            instance: AuthCapabilityInstance(32),
            operation: Box::new(|_terminal| {
                Box::pin(async { Some(AuthTaskOutcome::Signer(AuthSignerOutcome::Unavailable)) })
            }),
        };
        assert!(tasks.schedule(signer).is_none());
        let signer = tasks
            .released(recv_release(&rx))
            .expect("signer waits for exact release");
        launch_auth_task(signer, &mut tasks, rt.handle(), &inbox);
        let completion = match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::AuthTaskCompleted(completion) => completion,
            _ => panic!("signer completion expected"),
        };
        assert!(matches!(
            tasks.finish(completion),
            Some(EngineMsg::AuthSignerCompleted(current, ..)) if current == signer_token
        ));
        assert!(tasks.released(recv_release(&rx)).is_none());
        assert_eq!(tasks.len(), 0);
    }

    #[test]
    fn cancel_hook_round_trip_does_not_block_replacement_or_shutdown_drain() {
        let rt = test_runtime();
        let (inbox, rx) = std::sync::mpsc::channel();
        let mut tasks = AuthTaskRegistry::default();
        let request_token = token();
        let (hook_entered_tx, hook_entered_rx) = cb::bounded(1);
        let (hook_reply_tx, hook_reply_rx) = cb::bounded(1);
        let (_pending_tx, pending) = AuthPolicyOp::pending_channel_with_cancel(move || {
            let _ = hook_entered_tx.send(());
            let _ = hook_reply_rx.recv();
        });
        launch_auth_task(
            PendingAuthTask {
                token: request_token.clone(),
                capability: AuthCapability::Policy,
                instance: AuthCapabilityInstance(41),
                operation: Box::new(move |terminal| {
                    Box::pin(async move {
                        match pending {
                            AuthPolicyOp::Pending(pending) => {
                                let canceller = pending.canceller();
                                terminal.arm(Box::new(move || canceller.cancel()));
                                pending
                                    .await
                                    .map(|_| AuthTaskOutcome::Policy(AuthPolicyOutcome::Allow))
                            }
                            AuthPolicyOp::Ready(_) => unreachable!(),
                        }
                    })
                }),
            },
            &mut tasks,
            rt.handle(),
            &inbox,
        );
        tasks.cancel_epoch(&request_token.epoch);
        hook_entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("foreign cancel hook entered");
        assert_eq!(
            tasks.len(),
            1,
            "engine cancellation never waits on app code"
        );
        hook_reply_tx.send(()).unwrap();
        assert!(tasks.released(recv_release(&rx)).is_none());
        tasks.shutdown();
    }

    #[test]
    fn absent_policy_and_signer_fail_closed_with_the_exact_token() {
        let rt = test_runtime();
        let (pool_tx, _pool_rx) = std::sync::mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), pool_tx).unwrap();
        let registry = SignerRegistry::default();
        let policies = AuthPolicyRegistry::default();
        let mut tasks = AuthTaskRegistry::default();
        let (inbox, rx) = std::sync::mpsc::channel();
        let policy_token = token();
        dispatch(
            AuthEffect::RequestPolicy {
                token: policy_token.clone(),
                expected_pubkey: match policy_token.epoch.session.access {
                    AccessContext::Nip42(pubkey) => pubkey,
                    AccessContext::Public => unreachable!(),
                },
                challenge: "challenge".to_string(),
            },
            &pool,
            &registry,
            &policies,
            &mut tasks,
            rt.handle(),
            &inbox,
            &mut |token, capability, instance| {
                let _ = inbox.send(Cmd::Engine(EngineMsg::AuthCapabilityBound {
                    token,
                    capability,
                    instance,
                }));
            },
        );
        assert!(matches!(
            rx.recv().unwrap(),
            Cmd::Engine(EngineMsg::AuthPolicyCompleted(
                current,
                None,
                AuthPolicyOutcome::Unavailable
            )) if current == policy_token
        ));

        let sign_token = token();
        let unsigned =
            nostr::EventBuilder::auth("challenge", sign_token.epoch.session.relay.clone()).build(
                match sign_token.epoch.session.access {
                    AccessContext::Nip42(pubkey) => pubkey,
                    AccessContext::Public => unreachable!(),
                },
            );
        dispatch(
            AuthEffect::RequestSignature {
                token: sign_token.clone(),
                unsigned: Box::new(unsigned),
            },
            &pool,
            &registry,
            &policies,
            &mut tasks,
            rt.handle(),
            &inbox,
            &mut |token, capability, instance| {
                let _ = inbox.send(Cmd::Engine(EngineMsg::AuthCapabilityBound {
                    token,
                    capability,
                    instance,
                }));
            },
        );
        assert!(matches!(
            rx.recv().unwrap(),
            Cmd::Engine(EngineMsg::AuthSignerCompleted(
                current,
                None,
                AuthSignerOutcome::Unavailable
            )) if current == sign_token
        ));

        let send_token = token();
        let send_keys = Keys::generate();
        let event = nostr::EventBuilder::auth("challenge", send_token.epoch.session.relay.clone())
            .sign_with_keys(&send_keys)
            .unwrap();
        dispatch(
            AuthEffect::Send {
                token: send_token.clone(),
                epoch: send_token.epoch.clone(),
                event: Box::new(event),
            },
            &pool,
            &registry,
            &policies,
            &mut tasks,
            rt.handle(),
            &inbox,
            &mut |token, capability, instance| {
                let _ = inbox.send(Cmd::Engine(EngineMsg::AuthCapabilityBound {
                    token,
                    capability,
                    instance,
                }));
            },
        );
        assert!(matches!(
            rx.recv().unwrap(),
            Cmd::Engine(EngineMsg::AuthSendCompleted(
                current,
                AuthSendOutcome::Unavailable
            )) if current == send_token
        ));
        tasks.shutdown();
        pool.shutdown();
    }

    #[test]
    fn exact_connected_auth_flush_reports_the_exact_token_accepted() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (wire_tx, wire_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let message = socket.read().unwrap();
            let _ = wire_tx.send(message.into_text().unwrap().to_string());
        });

        let relay = RelayUrl::parse(&format!("ws://{address}")).unwrap();
        let keys = Keys::generate();
        let session = RelaySessionKey::new(relay.clone(), AccessContext::Nip42(keys.public_key()));
        let (pool_tx, pool_rx) = std::sync::mpsc::channel();
        // Issue #519/#524 resolved-IP admission refuses a 127.0.0.1 dial
        // unless the operator opts that host in — the same opt-in the
        // transport's own real-socket tests use (`test_pool_config`). This
        // postdates the mega base this reducer was ported from.
        let pool = Pool::new(
            PoolConfig {
                allowed_local_hosts: std::sync::Arc::new(std::collections::BTreeSet::from([
                    "127.0.0.1".to_string(),
                ])),
                ..PoolConfig::default()
            },
            pool_tx,
        )
        .unwrap();
        let opened = pool.ensure_session(&session).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let connected = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = pool_rx.recv_timeout(remaining).unwrap();
            if let PoolEvent::Connected {
                handle,
                session: observed,
            } = event
            {
                assert_eq!(observed, session);
                break handle;
            }
        };
        assert_eq!(connected, opened);

        let token = AuthOpToken {
            epoch: AuthEpoch {
                handle: connected,
                session: session.clone(),
                sequence: 41,
            },
            sequence: 42,
        };
        let event = nostr::EventBuilder::auth("exact-challenge", relay)
            .sign_with_keys(&keys)
            .unwrap();
        let registry = SignerRegistry::default();
        let policies = AuthPolicyRegistry::default();
        let mut tasks = AuthTaskRegistry::default();
        let rt = test_runtime();
        let (inbox, rx) = std::sync::mpsc::channel();
        dispatch(
            AuthEffect::Send {
                token: token.clone(),
                epoch: token.epoch.clone(),
                event: Box::new(event),
            },
            &pool,
            &registry,
            &policies,
            &mut tasks,
            rt.handle(),
            &inbox,
            &mut |token, capability, instance| {
                let _ = inbox.send(Cmd::Engine(EngineMsg::AuthCapabilityBound {
                    token,
                    capability,
                    instance,
                }));
            },
        );

        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Cmd::Engine(EngineMsg::AuthSendCompleted(
                current,
                AuthSendOutcome::Accepted
            )) if current == token
        ));
        let wire = wire_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(wire.starts_with("[\"AUTH\","));
        assert!(wire.contains("exact-challenge"));

        tasks.shutdown();
        pool.shutdown();
        server.join().unwrap();
    }
}
