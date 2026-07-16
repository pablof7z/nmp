//! Runtime ownership for reducer-owned NIP-42 operations.
//!
//! The reducer owns AUTH truth. This module owns only finite capability
//! registries, exact operation cancellation, and transport execution. Every
//! policy/signer task is bound to one checked capability instance before any
//! callback starts, and at most one such task is live for an exact relay
//! session.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crossbeam_channel as cb;
use nmp_signer::{
    pending_signer_cancellation, PendingSignerCancel, PendingSignerCancelled, SignerError, SignerOp,
};
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
type PendingPolicySlot = Mutex<Option<cb::Sender<PolicyResult>>>;
type PendingPolicyCancel = Box<dyn FnOnce() + Send + 'static>;

/// One-shot completion door for a pending [`AuthPolicyOp`].
pub struct AuthPolicyPendingSender {
    sender: Arc<PendingPolicySlot>,
}

impl Clone for AuthPolicyPendingSender {
    fn clone(&self) -> Self {
        Self {
            sender: Arc::clone(&self.sender),
        }
    }
}

impl AuthPolicyPendingSender {
    pub fn resolve(&self, result: PolicyResult) -> Result<(), AuthPolicyResolveError> {
        let Some(sender) = self
            .sender
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        else {
            return Err(AuthPolicyResolveError::AlreadyResolved(result));
        };
        sender
            .send(result)
            .map_err(|error| AuthPolicyResolveError::ReceiverDropped(error.0))
    }
}

#[derive(Debug)]
pub enum AuthPolicyResolveError {
    AlreadyResolved(PolicyResult),
    ReceiverDropped(PolicyResult),
}

pub struct PendingAuthPolicyOp {
    receiver: Option<cb::Receiver<PolicyResult>>,
    cancel: Option<PendingPolicyCancel>,
}

impl PendingAuthPolicyOp {
    fn recv_or_cancel(mut self, cancelled: cb::Receiver<()>) -> Option<PolicyResult> {
        let receiver = self
            .receiver
            .take()
            .expect("pending policy receiver consumed once");
        let outcome = cb::select_biased! {
            recv(cancelled) -> _ => self.cancel_or_receive(&receiver),
            recv(receiver) -> result => {
                self.cancel = None;
                Some(result.unwrap_or(Err(AuthPolicyError::Unavailable)))
            }
        };
        outcome
    }

    /// Terminal cancellation first consumes an answer already committed to
    /// the channel. Only a successful atomic cancellation may abandon the
    /// receiver. An adapter whose resolver already owns completion keeps this
    /// callback blocked until the result is sent or its sender disconnects;
    /// the receiver stays alive for that entire handshake.
    fn cancel_or_receive(&mut self, receiver: &cb::Receiver<PolicyResult>) -> Option<PolicyResult> {
        match receiver.try_recv() {
            Ok(result) => {
                self.cancel = None;
                Some(result)
            }
            Err(cb::TryRecvError::Disconnected) => {
                self.cancel = None;
                Some(Err(AuthPolicyError::Unavailable))
            }
            Err(cb::TryRecvError::Empty) => {
                if let Some(cancel) = self.cancel.take() {
                    cancel();
                }
                match receiver.try_recv() {
                    Ok(result) => Some(result),
                    Err(cb::TryRecvError::Disconnected) => Some(Err(AuthPolicyError::Unavailable)),
                    Err(cb::TryRecvError::Empty) => None,
                }
            }
        }
    }
}

impl Drop for PendingAuthPolicyOp {
    fn drop(&mut self) {
        let Some(receiver) = self.receiver.take() else {
            return;
        };
        // Dropping the pending operation is the same terminal-cancel door as
        // an epoch/capability signal. A resolver that already owns completion
        // must still be allowed to send before the receiver is released.
        let _ = self.cancel_or_receive(&receiver);
        self.cancel = None;
    }
}

impl PendingAuthPolicyOp {
    fn with_cancel_handshake(
        receiver: cb::Receiver<PolicyResult>,
        cancel: Option<PendingPolicyCancel>,
    ) -> Self {
        Self {
            receiver: Some(receiver),
            cancel,
        }
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
        let (sender, receiver) = cb::bounded(1);
        (
            AuthPolicyPendingSender {
                sender: Arc::new(Mutex::new(Some(sender))),
            },
            Self::Pending(PendingAuthPolicyOp::with_cancel_handshake(receiver, cancel)),
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

struct AuthTaskTerminal {
    state: AtomicU8,
    signer_cancel: PendingSignerCancel,
    policy_cancel: cb::Sender<()>,
}

impl AuthTaskTerminal {
    fn new() -> (Arc<Self>, PendingSignerCancelled, cb::Receiver<()>) {
        let (signer_cancel, signer_cancelled) = pending_signer_cancellation();
        let (policy_cancel, policy_cancelled) = cb::bounded(1);
        (
            Arc::new(Self {
                state: AtomicU8::new(AUTH_TASK_OPEN),
                signer_cancel,
                policy_cancel,
            }),
            signer_cancelled,
            policy_cancelled,
        )
    }

    fn is_open(&self) -> bool {
        self.state.load(Ordering::Acquire) == AUTH_TASK_OPEN
    }

    fn cancel(&self) -> bool {
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
        self.signer_cancel.cancel();
        let _ = self.policy_cancel.try_send(());
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

type AuthTaskOperation = Box<
    dyn FnOnce(
            Arc<AuthTaskTerminal>,
            PendingSignerCancelled,
            cb::Receiver<()>,
        ) -> Option<AuthTaskOutcome>
        + Send
        + 'static,
>;

pub(super) struct PendingAuthTask {
    token: AuthOpToken,
    capability: AuthCapability,
    instance: AuthCapabilityInstance,
    operation: AuthTaskOperation,
}

pub(super) struct AuthTaskRegistry {
    active: HashMap<nmp_grammar::RelaySessionKey, ActiveAuthTask>,
    pending: HashMap<nmp_grammar::RelaySessionKey, PendingAuthTask>,
    releases: HashMap<nmp_executor::ReleaseId, AuthTaskRelease>,
    next_release_id: Option<u64>,
    release_sender: Sender<nmp_executor::ReleaseId>,
}

impl Default for AuthTaskRegistry {
    fn default() -> Self {
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        drop(release_receiver);
        Self::with_release_sender(release_sender)
    }
}

impl AuthTaskRegistry {
    pub(super) fn with_release_sender(release_sender: Sender<nmp_executor::ReleaseId>) -> Self {
        Self {
            active: HashMap::new(),
            pending: HashMap::new(),
            releases: HashMap::new(),
            next_release_id: Some(1),
            release_sender,
        }
    }

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

    fn started(&mut self, task: ActiveAuthTask) -> Option<nmp_executor::ReleaseId> {
        let value = self.next_release_id?;
        self.next_release_id = value.checked_add(1);
        let release_id = nmp_executor::ReleaseId::new(value);
        self.releases.insert(
            release_id,
            AuthTaskRelease {
                session: task.token.epoch.session.clone(),
                terminal: Arc::clone(&task.terminal),
            },
        );
        self.active.insert(task.token.epoch.session.clone(), task);
        Some(release_id)
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

    pub(super) fn released(
        &mut self,
        release_id: nmp_executor::ReleaseId,
    ) -> Option<PendingAuthTask> {
        let release = self.releases.remove(&release_id)?;
        let exact = self
            .active
            .get(&release.session)
            .is_some_and(|task| Arc::ptr_eq(&task.terminal, &release.terminal));
        if !exact {
            return None;
        }
        self.active.remove(&release.session);
        self.pending.remove(&release.session)
    }

    pub(super) fn shutdown(&mut self) {
        for task in self.active.values() {
            task.terminal.cancel();
        }
        self.pending.clear();
    }

    pub(super) fn is_empty(&self) -> bool {
        self.active.is_empty() && self.pending.is_empty() && self.releases.is_empty()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.active.len() + self.pending.len()
    }
}

pub(super) struct AuthTaskRelease {
    session: nmp_grammar::RelaySessionKey,
    terminal: Arc<AuthTaskTerminal>,
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
    native_tasks: &nmp_executor::Executor,
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
                native_tasks,
                inbox,
                bind,
                move |terminal, _, policy_cancelled| {
                    let op = policy
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .evaluate(request);
                    let result = match op {
                        AuthPolicyOp::Ready(result) => Some(result),
                        AuthPolicyOp::Pending(pending) => pending.recv_or_cancel(policy_cancelled),
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
                native_tasks,
                inbox,
                bind,
                move |terminal, signer_cancelled, _| {
                    let op = signer
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .sign(*unsigned);
                    let result = match op {
                        SignerOp::Ready(result) => Some(result),
                        SignerOp::Pending(pending) => pending.recv_or_cancel(signer_cancelled),
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
    native_tasks: &nmp_executor::Executor,
    inbox: &Sender<Cmd>,
    bind: &mut impl FnMut(AuthOpToken, AuthCapability, AuthCapabilityInstance),
    operation: impl FnOnce(
            Arc<AuthTaskTerminal>,
            PendingSignerCancelled,
            cb::Receiver<()>,
        ) -> Option<AuthTaskOutcome>
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
        launch_auth_task(ready, tasks, native_tasks, inbox);
    }
}

pub(super) fn launch_auth_task(
    task: PendingAuthTask,
    tasks: &mut AuthTaskRegistry,
    native_tasks: &nmp_executor::Executor,
    inbox: &Sender<Cmd>,
) {
    let PendingAuthTask {
        token,
        capability,
        instance,
        operation,
    } = task;
    let component = match capability {
        AuthCapability::Policy => "auth-policy",
        AuthCapability::Signer => "auth-signer",
    };
    let reservation = match native_tasks.reserve(component) {
        Ok(reservation) => reservation,
        Err(error) => {
            enqueue_failure(
                inbox,
                token,
                capability,
                instance,
                format!(
                    "{component} executor saturated at capacity {}",
                    error.capacity
                ),
            );
            return;
        }
    };
    let (terminal, signer_cancelled, policy_cancelled) = AuthTaskTerminal::new();
    let shutdown_terminal = Arc::clone(&terminal);
    let starter = match reservation.start_with_cancel(move || {
        shutdown_terminal.cancel();
    }) {
        Ok(starter) => starter,
        Err(error) => {
            enqueue_failure(inbox, token, capability, instance, error.to_string());
            return;
        }
    };
    let Some(release_id) = tasks.started(ActiveAuthTask {
        token: token.clone(),
        capability,
        instance,
        terminal: Arc::clone(&terminal),
    }) else {
        enqueue_failure(
            inbox,
            token,
            capability,
            instance,
            "AUTH task release id namespace exhausted".to_string(),
        );
        return;
    };

    let completion_inbox = inbox.clone();
    let release_sender = tasks.release_sender.clone();
    starter.run_with_release_signal(
        move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if !terminal.is_open() {
                    return None;
                }
                operation(Arc::clone(&terminal), signer_cancelled, policy_cancelled)
            }))
            .unwrap_or_else(|_| {
                Some(match capability {
                    AuthCapability::Policy => AuthTaskOutcome::Policy(AuthPolicyOutcome::Error {
                        reason: "AUTH policy panicked".to_string(),
                    }),
                    AuthCapability::Signer => AuthTaskOutcome::Signer(AuthSignerOutcome::Error {
                        reason: "AUTH signer panicked".to_string(),
                    }),
                })
            });
            if let Some(outcome) = outcome.filter(|_| terminal.complete()) {
                let completion = AuthTaskCompletion {
                    token,
                    capability,
                    instance,
                    terminal,
                    outcome,
                };
                let _ = completion_inbox.send(Cmd::AuthTaskCompleted(completion));
            }
        },
        release_sender,
        release_id,
    );
}

fn enqueue_failure(
    inbox: &Sender<Cmd>,
    token: AuthOpToken,
    capability: AuthCapability,
    instance: AuthCapabilityInstance,
    reason: String,
) {
    let outcome = match capability {
        AuthCapability::Policy => EngineMsg::AuthPolicyCompleted(
            token,
            Some(instance),
            AuthPolicyOutcome::Error { reason },
        ),
        AuthCapability::Signer => EngineMsg::AuthSignerCompleted(
            token,
            Some(instance),
            AuthSignerOutcome::Error { reason },
        ),
    };
    let _ = inbox.send(Cmd::Engine(outcome));
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

    fn task_registry() -> (
        AuthTaskRegistry,
        std::sync::mpsc::Receiver<nmp_executor::ReleaseId>,
    ) {
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        (
            AuthTaskRegistry::with_release_sender(release_sender),
            release_receiver,
        )
    }

    #[test]
    fn terminal_cancel_consumes_a_result_already_queued() {
        let (completion, operation) = AuthPolicyOp::pending_channel_with_cancel(|| {
            panic!("queued completion must suppress cancellation")
        });
        let AuthPolicyOp::Pending(pending) = operation else {
            unreachable!()
        };
        completion.resolve(Ok(AuthPolicyDecision::Allow)).unwrap();
        let (cancelled_tx, cancelled_rx) = cb::bounded(1);
        cancelled_tx.send(()).unwrap();

        assert_eq!(
            pending.recv_or_cancel(cancelled_rx),
            Some(Ok(AuthPolicyDecision::Allow))
        );
    }

    #[test]
    fn terminal_cancel_wins_once_before_resolve_and_drops_receiver_truthfully() {
        let cancellations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&cancellations);
        let (completion, operation) = AuthPolicyOp::pending_channel_with_cancel(move || {
            observed.fetch_add(1, Ordering::AcqRel);
        });
        let AuthPolicyOp::Pending(pending) = operation else {
            unreachable!()
        };
        let (cancelled_tx, cancelled_rx) = cb::bounded(1);
        cancelled_tx.send(()).unwrap();

        assert_eq!(pending.recv_or_cancel(cancelled_rx), None);
        assert_eq!(cancellations.load(Ordering::Acquire), 1);
        assert!(matches!(
            completion.resolve(Ok(AuthPolicyDecision::Allow)),
            Err(AuthPolicyResolveError::ReceiverDropped(Ok(
                AuthPolicyDecision::Allow
            )))
        ));
    }

    #[test]
    fn losing_cancel_waits_for_the_resolver_owned_result() {
        let (resolving_tx, resolving_rx) = cb::bounded(1);
        let (release_tx, release_rx) = cb::bounded(1);
        let (completion, operation) = AuthPolicyOp::pending_channel_with_cancel(move || {
            resolving_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        let AuthPolicyOp::Pending(pending) = operation else {
            unreachable!()
        };
        let (cancelled_tx, cancelled_rx) = cb::bounded(1);
        cancelled_tx.send(()).unwrap();
        let waiter = std::thread::spawn(move || pending.recv_or_cancel(cancelled_rx));

        resolving_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancel handshake must observe resolver ownership");
        completion
            .resolve(Ok(AuthPolicyDecision::Allow))
            .expect("losing cancellation must retain the receiver");
        release_tx.send(()).unwrap();
        assert_eq!(waiter.join().unwrap(), Some(Ok(AuthPolicyDecision::Allow)));
    }

    #[test]
    fn policy_is_bound_before_callback_and_completes_the_exact_instance() {
        let (pool_tx, _pool_rx) = std::sync::mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), pool_tx).unwrap();
        let signers = SignerRegistry::default();
        let mut policies = AuthPolicyRegistry::default();
        let (mut tasks, release_rx) = task_registry();
        let executor = nmp_executor::Executor::new(1).unwrap();
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
            &executor,
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
        let release = release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(tasks.released(release).is_none());
        assert_eq!(tasks.len(), 0);

        pool.shutdown();
        executor.shutdown();
    }

    #[test]
    fn replacement_and_epoch_cancel_drain_pending_policy_tasks() {
        let (pool_tx, _pool_rx) = std::sync::mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), pool_tx).unwrap();
        let signers = SignerRegistry::default();
        let mut policies = AuthPolicyRegistry::default();
        let (mut tasks, release_rx) = task_registry();
        let executor = nmp_executor::Executor::new(1).unwrap();
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
            &executor,
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
        let release = release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(tasks.released(release).is_none());
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
        executor.shutdown();
    }

    #[test]
    fn cancel_and_completion_have_one_linearization_winner() {
        let (completed_first, _, _) = AuthTaskTerminal::new();
        assert!(completed_first.complete());
        assert!(!completed_first.cancel());

        let (cancelled_first, _, _) = AuthTaskTerminal::new();
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
    fn capacity_one_releases_policy_before_starting_signer_phase() {
        let executor = nmp_executor::Executor::new(1).unwrap();
        let (inbox, rx) = std::sync::mpsc::channel();
        let (mut tasks, release_rx) = task_registry();
        let policy_token = token();
        launch_auth_task(
            PendingAuthTask {
                token: policy_token.clone(),
                capability: AuthCapability::Policy,
                instance: AuthCapabilityInstance(31),
                operation: Box::new(|_, _, _| {
                    Some(AuthTaskOutcome::Policy(AuthPolicyOutcome::Allow))
                }),
            },
            &mut tasks,
            &executor,
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
            operation: Box::new(|_, _, _| {
                Some(AuthTaskOutcome::Signer(AuthSignerOutcome::Unavailable))
            }),
        };
        assert!(tasks.schedule(signer).is_none());
        let release = release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let signer = tasks
            .released(release)
            .expect("signer waits for exact release");
        launch_auth_task(signer, &mut tasks, &executor, &inbox);
        let completion = match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::AuthTaskCompleted(completion) => completion,
            _ => panic!("signer completion expected"),
        };
        assert!(matches!(
            tasks.finish(completion),
            Some(EngineMsg::AuthSignerCompleted(current, ..)) if current == signer_token
        ));
        let release = release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(tasks.released(release).is_none());
        assert_eq!(executor.census().admitted, 0);
        executor.shutdown();
    }

    #[test]
    fn cancel_hook_round_trip_does_not_block_replacement_or_shutdown_drain() {
        let executor = nmp_executor::Executor::new(1).unwrap();
        let (inbox, _rx) = std::sync::mpsc::channel();
        let (mut tasks, release_rx) = task_registry();
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
                operation: Box::new(move |_, _, cancelled| match pending {
                    AuthPolicyOp::Pending(pending) => pending
                        .recv_or_cancel(cancelled)
                        .map(|_| AuthTaskOutcome::Policy(AuthPolicyOutcome::Allow)),
                    AuthPolicyOp::Ready(_) => unreachable!(),
                }),
            },
            &mut tasks,
            &executor,
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
        let release = release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(tasks.released(release).is_none());
        tasks.shutdown();
        executor.shutdown();
        assert_eq!(executor.census().admitted, 0);
    }

    #[test]
    fn absent_policy_and_signer_fail_closed_with_the_exact_token() {
        let (pool_tx, _pool_rx) = std::sync::mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), pool_tx).unwrap();
        let registry = SignerRegistry::default();
        let policies = AuthPolicyRegistry::default();
        let mut tasks = AuthTaskRegistry::default();
        let executor = nmp_executor::Executor::new(2).unwrap();
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
            &executor,
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
            &executor,
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
            &executor,
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
        executor.shutdown();
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
        let executor = nmp_executor::Executor::new(1).unwrap();
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
            &executor,
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
        executor.shutdown();
        server.join().unwrap();
    }
}
