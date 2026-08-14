use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use nmp_signer::{PendingSignerOp, SignerOp, SignerSignedEvent};
use nostr::{Event as SignedEvent, EventId, UnsignedEvent};

use super::{decode_signed_event, Cmd, Handle, RuntimeSessionState};

/// Typed outcome vocabulary for the governed sign-only operation. This is
/// deliberately separate from write receipts: signing here never accepts a
/// write intent, mutates canonical storage, creates a delivery lane, or
/// publishes to a relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignEventError {
    NoCurrentSigningProvider,
    InvalidRequest { reason: String },
    SignerUnavailable { reason: String },
    SignerRejected { reason: String },
    InvalidSignerOutput { reason: String },
    EngineClosed,
    Cancelled,
}

pub(super) type SignEventCompletion =
    Box<dyn FnOnce(Result<SignedEvent, SignEventError>) + Send + 'static>;

#[repr(u8)]
#[derive(Clone, Copy)]
enum SignEventState {
    Open,
    Cancelled,
    Resolved,
}

thread_local! {
    /// #704: set on a per-operation sign-event completion thread to the exact
    /// operation id it is running. `EngineThread::join()` reads it so a
    /// completion closure that calls `join()` reentrantly exempts only its own
    /// operation from the shutdown drain (replacing the executor `TaskId`
    /// mechanism, which is gone).
    static SIGN_EVENT_COMPLETION_OP: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

pub(super) fn completion_operation() -> Option<u64> {
    SIGN_EVENT_COMPLETION_OP.with(std::cell::Cell::get)
}

/// One linearization point shared by caller cancellation, engine shutdown,
/// runtime shutdown, and signer completion. Cancellation claims `Open ->
/// Cancelled` and fires the bound cancel action (the pending op's canceller for
/// a remote signer; a no-op for a ready local signer).
struct SignEventTerminal {
    state: AtomicU8,
    cancel: Box<dyn Fn() + Send + Sync>,
}

impl SignEventTerminal {
    fn new(cancel: Box<dyn Fn() + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(SignEventState::Open as u8),
            cancel,
        })
    }

    fn cancel(&self) -> bool {
        if self
            .state
            .compare_exchange(
                SignEventState::Open as u8,
                SignEventState::Cancelled as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        (self.cancel)();
        true
    }

    fn resolve(&self) -> bool {
        self.state
            .compare_exchange(
                SignEventState::Open as u8,
                SignEventState::Resolved as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

pub(super) struct SignEventRegistration {
    id: u64,
    terminal: Arc<SignEventTerminal>,
}

struct ActiveSignEvent {
    terminal: Arc<SignEventTerminal>,
}

pub(super) struct ActiveSignEvents {
    next_id: u64,
    cancellations: HashMap<u64, ActiveSignEvent>,
}

impl Default for ActiveSignEvents {
    fn default() -> Self {
        Self {
            next_id: 1,
            cancellations: HashMap::new(),
        }
    }
}

impl ActiveSignEvents {
    pub(super) fn admit(
        &mut self,
        registry: &RuntimeSessionState,
        runtime_handle: &tokio::runtime::Handle,
        inbox: &Sender<Cmd>,
        unsigned: UnsignedEvent,
        completion: SignEventCompletion,
        reply: Sender<Result<SignEventRegistration, SignEventError>>,
    ) {
        let Some(author) = registry.current_pubkey else {
            let _ = reply.send(Err(SignEventError::NoCurrentSigningProvider));
            return;
        };
        if unsigned.pubkey != author {
            let _ = reply.send(Err(SignEventError::InvalidRequest {
                reason: "request author does not match the current account".to_string(),
            }));
            return;
        }
        let expected_id = match validate_sign_request(&unsigned) {
            Ok(expected_id) => expected_id,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let Some(signer_op) = registry.sign(unsigned.clone()) else {
            let _ = reply.send(Err(SignEventError::NoCurrentSigningProvider));
            return;
        };

        let operation_id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);

        // #704: the SIGNING WAIT holds no thread. A ready local signer
        // has its result now; a pending remote signer is awaited by an
        // async task on the adapter runtime. Cancellation fires the
        // pending op's canceller (a no-op for a ready result); the
        // foreign `completion` — which may block and may call
        // `Engine::join()` reentrantly — always runs on a FRESH per-op
        // OS thread, never the runtime or the reducer.
        let (cancel_action, signer_source): (Box<dyn Fn() + Send + Sync>, SignEventSignerResult) =
            match signer_op {
                SignerOp::Ready(result) => (
                    Box::new(|| {}),
                    SignEventSignerResult::Ready(Box::new(result)),
                ),
                SignerOp::Pending(pending) => {
                    let canceller = pending.canceller();
                    (
                        Box::new(move || canceller.cancel()),
                        SignEventSignerResult::Pending(pending),
                    )
                }
            };
        let terminal = SignEventTerminal::new(cancel_action);

        self.cancellations.insert(
            operation_id,
            ActiveSignEvent {
                terminal: Arc::clone(&terminal),
            },
        );
        if reply
            .send(Ok(SignEventRegistration {
                id: operation_id,
                terminal: Arc::clone(&terminal),
            }))
            .is_err()
        {
            self.cancellations.remove(&operation_id);
            terminal.cancel();
            return;
        }

        let inbox = inbox.clone();
        match signer_source {
            SignEventSignerResult::Ready(result) => {
                spawn_sign_event_completion(
                    inbox,
                    operation_id,
                    terminal,
                    unsigned,
                    expected_id,
                    Some(*result),
                    completion,
                );
            }
            SignEventSignerResult::Pending(pending) => {
                // The signing wait is async; the (possibly-blocking)
                // foreign completion is delivered on a per-op thread
                // whether the await resolves OR the task's future is
                // dropped at runtime shutdown (the dispatch Drop guard).
                let dispatch = SignEventCompletionDispatch {
                    inbox,
                    operation_id,
                    terminal,
                    unsigned,
                    expected_id,
                    completion: Some(completion),
                    signer_result: None,
                };
                runtime_handle.spawn(async move {
                    let mut dispatch = dispatch;
                    let result = pending.await;
                    dispatch.signer_result = Some(result);
                    // drop(dispatch) here spawns the completion thread.
                });
            }
        }
    }

    pub(super) fn cancel(&mut self, id: u64) {
        self.remove_and_cancel(id);
    }

    pub(super) fn finish(&mut self, id: u64) {
        self.cancellations.remove(&id);
    }

    fn remove_and_cancel(&mut self, id: u64) {
        if let Some(active) = self.cancellations.remove(&id) {
            active.terminal.cancel();
        }
    }

    pub(super) fn exempt_from_shutdown_drain(&mut self, id: u64) {
        self.cancellations.remove(&id);
    }

    pub(super) fn cancel_for_shutdown(&self) {
        for active in self.cancellations.values() {
            active.terminal.cancel();
        }
    }

    pub(super) fn drain_for_shutdown(&mut self) {
        for (_, active) in self.cancellations.drain() {
            active.terminal.cancel();
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.cancellations.is_empty()
    }
}

/// #704: run one foreign sign-event `completion` closure on a FRESH dedicated
/// OS thread spawned for that single in-flight app operation. The closure may
/// block indefinitely and may call `Engine::join()` reentrantly (the
/// reentrant-join tests) — running it on the shared runtime would stall the
/// fixed workers, and a reentrant `join()` from a worker would deadlock tokio.
/// The thread advertises its operation id via `SIGN_EVENT_COMPLETION_OP` so
/// `join()` can exempt exactly this operation, and posts `SignEventFinished`
/// via a drop guard on the way out (panic-safe).
#[allow(clippy::too_many_arguments)]
fn spawn_sign_event_completion(
    inbox: Sender<Cmd>,
    operation_id: u64,
    terminal: Arc<SignEventTerminal>,
    unsigned: UnsignedEvent,
    expected_id: EventId,
    signer_result: Option<Result<SignerSignedEvent, nmp_signer::SignerError>>,
    completion: SignEventCompletion,
) {
    let thread_inbox = inbox.clone();
    let spawned = thread::Builder::new()
        .name("nmp-sign-event-completion".to_string())
        .spawn(move || {
            nmp_transport::thread_census::run_counted_thread(move || {
                SIGN_EVENT_COMPLETION_OP.with(|op| op.set(Some(operation_id)));
                let _finished = SignEventFinishedGuard {
                    inbox: thread_inbox,
                    operation_id,
                };
                let result = match signer_result {
                    Some(result) if terminal.resolve() => result
                        .and_then(decode_signed_event)
                        .map_err(signer_error)
                        .and_then(|signed| validate_signer_output(&unsigned, expected_id, signed)),
                    Some(_) | None => Err(SignEventError::Cancelled),
                };
                completion(result);
            });
        });
    if spawned.is_err() {
        // OS thread exhaustion (astronomically rare): the failed spawn dropped
        // the completion closure without calling it, so the caller observes a
        // disconnected result. Clear the operation from the shutdown drain.
        let _ = inbox.send(Cmd::SignEventFinished(operation_id));
    }
}

/// #704: owns the foreign sign-event `completion` while the async signing wait
/// is outstanding. When the awaiting task resolves it sets `signer_result` and
/// drops; when the task's future is instead dropped (runtime shutdown /
/// cancellation) `signer_result` stays `None`. Either way `Drop` runs the
/// completion exactly once on a fresh per-op OS thread (delivering a signed
/// event, a signer error, or `Cancelled`), never leaving the foreign closure
/// uncalled.
struct SignEventCompletionDispatch {
    inbox: Sender<Cmd>,
    operation_id: u64,
    terminal: Arc<SignEventTerminal>,
    unsigned: UnsignedEvent,
    expected_id: EventId,
    completion: Option<SignEventCompletion>,
    signer_result: Option<Result<SignerSignedEvent, nmp_signer::SignerError>>,
}

impl Drop for SignEventCompletionDispatch {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            spawn_sign_event_completion(
                self.inbox.clone(),
                self.operation_id,
                Arc::clone(&self.terminal),
                self.unsigned.clone(),
                self.expected_id,
                self.signer_result.take(),
                completion,
            );
        }
    }
}

struct SignEventFinishedGuard {
    inbox: Sender<Cmd>,
    operation_id: u64,
}

impl Drop for SignEventFinishedGuard {
    fn drop(&mut self) {
        let _ = self.inbox.send(Cmd::SignEventFinished(self.operation_id));
    }
}

impl std::fmt::Display for SignEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCurrentSigningProvider => {
                f.write_str("the current account has no available signing provider")
            }
            Self::InvalidRequest { reason } => write!(f, "invalid sign request: {reason}"),
            Self::SignerUnavailable { reason } => write!(f, "signer unavailable: {reason}"),
            Self::SignerRejected { reason } => write!(f, "signer rejected request: {reason}"),
            Self::InvalidSignerOutput { reason } => {
                write!(f, "signer returned invalid output: {reason}")
            }
            Self::EngineClosed => f.write_str("engine already shut down"),
            Self::Cancelled => f.write_str("sign operation cancelled"),
        }
    }
}

impl std::error::Error for SignEventError {}

fn signer_error(error: nmp_signer::SignerError) -> SignEventError {
    match error {
        nmp_signer::SignerError::InvalidResponse(reason) => {
            SignEventError::InvalidSignerOutput { reason }
        }
        nmp_signer::SignerError::Rejected(reason) => SignEventError::SignerRejected { reason },
        other => SignEventError::SignerUnavailable {
            reason: other.to_string(),
        },
    }
}

fn validate_sign_request(unsigned: &UnsignedEvent) -> Result<EventId, SignEventError> {
    let computed = EventId::new(
        &unsigned.pubkey,
        &unsigned.created_at,
        &unsigned.kind,
        &unsigned.tags,
        &unsigned.content,
    );
    if unsigned.id.is_some_and(|declared| declared != computed) {
        return Err(SignEventError::InvalidRequest {
            reason: "declared event id does not match the immutable body".to_string(),
        });
    }
    Ok(computed)
}

fn validate_signer_output(
    unsigned: &UnsignedEvent,
    expected_id: EventId,
    signed: SignedEvent,
) -> Result<SignedEvent, SignEventError> {
    if signed.id != expected_id
        || signed.pubkey != unsigned.pubkey
        || signed.created_at != unsigned.created_at
        || signed.kind != unsigned.kind
        || signed.tags != unsigned.tags
        || signed.content != unsigned.content
    {
        return Err(SignEventError::InvalidSignerOutput {
            reason: "signed event does not match the frozen body, author, or id".to_string(),
        });
    }
    signed
        .verify()
        .map_err(|error| SignEventError::InvalidSignerOutput {
            reason: format!("signature verification failed: {error}"),
        })?;
    Ok(signed)
}

/// One accepted sign-only operation. It owns no write receipt or durable
/// obligation: dropping it before completion cancels the exact signer RPC.
pub struct SignEventOperation {
    result: Option<Receiver<Result<SignedEvent, SignEventError>>>,
    cancel: SignEventCancel,
}

enum SignEventSignerResult {
    Ready(Box<Result<SignerSignedEvent, nmp_signer::SignerError>>),
    Pending(PendingSignerOp<SignerSignedEvent>),
}

impl SignEventOperation {
    pub fn recv(mut self) -> Result<SignedEvent, SignEventError> {
        self.result
            .take()
            .expect("sign-event result is consumed exactly once")
            .recv()
            .unwrap_or(Err(SignEventError::Cancelled))
    }

    #[must_use]
    pub fn cancel_handle(&self) -> SignEventCancel {
        self.cancel.clone()
    }
}

impl Drop for SignEventOperation {
    fn drop(&mut self) {
        if self.result.is_some() {
            self.cancel.cancel();
        }
    }
}

/// Idempotent cancellation token for one exact sign-only operation.
#[derive(Clone)]
pub struct SignEventCancel {
    inbox: Sender<Cmd>,
    id: u64,
    terminal: Arc<SignEventTerminal>,
}

impl SignEventCancel {
    pub fn cancel(&self) {
        if self.terminal.cancel() {
            let _ = self.inbox.send(Cmd::CancelSignEvent(self.id));
        }
    }
}

impl Handle {
    /// Ask the current account's registered signing provider to sign one exact event,
    /// without accepting a write or touching the canonical store/delivery state. A
    /// pending remote operation is cancellable through the returned handle and
    /// engine shutdown; #704 removed the admission slot — nothing is refused.
    pub fn sign_event(
        &self,
        unsigned: UnsignedEvent,
    ) -> Result<SignEventOperation, SignEventError> {
        let (completion_tx, completion_rx) = mpsc::channel();
        let cancel = self.sign_event_with_completion(unsigned, move |result| {
            let _ = completion_tx.send(result);
        })?;
        Ok(SignEventOperation {
            result: Some(completion_rx),
            cancel,
        })
    }

    #[doc(hidden)]
    pub fn sign_event_with_completion(
        &self,
        unsigned: UnsignedEvent,
        completion: impl FnOnce(Result<SignedEvent, SignEventError>) + Send + 'static,
    ) -> Result<SignEventCancel, SignEventError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::SignEvent {
                unsigned,
                completion: Box::new(completion),
                reply: reply_tx,
            })
            .map_err(|_| SignEventError::EngineClosed)?;
        let registration = reply_rx
            .recv()
            .map_err(|_| SignEventError::EngineClosed)??;
        Ok(SignEventCancel {
            inbox: self.inbox.clone(),
            id: registration.id,
            terminal: registration.terminal,
        })
    }
}
