//! The sign-only operation lifecycle (#1628), start to finish, in one owner.
//!
//! A sign-only operation is finite and self-contained: it accepts no write
//! intent, mutates no canonical storage, opens no delivery lane, and publishes
//! nothing. It exists from admission until exactly one terminal — a signed
//! event, a signer error, or `Cancelled` — reaches the caller's completion.
//!
//! [`ActiveSignEvents`] owns every part of that: the operation id space, the
//! live-operation map, the compare-and-set that arbitrates between caller
//! cancellation / engine shutdown / runtime shutdown / signer completion, and
//! the shutdown drain accounting. The engine loop calls its methods; it does
//! not reach into its map, its ids, or its terminals, and it never has to hold
//! any of that state itself.
//!
//! The owner's ONE inward dependency is the signing capability, and it is
//! spelled out in [`ActiveSignEvents::admit`]'s signature: the selected author
//! and `&SignerRegistry`, passed explicitly. It is deliberately not reached
//! through `RuntimeSessionState`'s `Deref`, which would let this edge read as
//! a coercion instead of a dependency. Nothing here touches `EngineCore`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use nmp_signer::{PendingSignerOp, SignerOp, SignerSignedEvent};
use nostr::{Event as SignedEvent, EventId, PublicKey, UnsignedEvent};

use super::{decode_signed_event, Cmd, Handle, SignerRegistry};

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
    /// operation id it is running. `EngineThread::join()` reads it (through
    /// [`completion_operation`]) so a completion closure that calls `join()`
    /// reentrantly exempts only its own operation from the shutdown drain
    /// (replacing the executor `TaskId` mechanism, which is gone).
    static SIGN_EVENT_COMPLETION_OP: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

/// The operation this thread is delivering a completion for, if it is one of
/// this owner's per-operation completion threads.
///
/// `EngineThread::join` needs exactly this one fact and nothing else about the
/// lifecycle, so the thread-local stays private here and `join` asks a
/// question instead of reading a variable.
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

/// What [`ActiveSignEvents::admit`] hands back to the caller's thread: the
/// operation's identity plus its share of the terminal.
pub(super) struct SignEventRegistration {
    id: u64,
    terminal: Arc<SignEventTerminal>,
}

struct ActiveSignEvent {
    terminal: Arc<SignEventTerminal>,
}

enum SignEventSignerResult {
    Ready(Box<Result<SignerSignedEvent, nmp_signer::SignerError>>),
    Pending(PendingSignerOp<SignerSignedEvent>),
}

/// Where an admitted operation's asynchronous work goes: the adapter runtime
/// that awaits a remote signer, and the inbox its completion posts back on.
///
/// Both belong to the runtime root and are borrowed for the length of one
/// admission — deliberately grouped, because "where the completion goes" is
/// one fact, and because keeping it apart from `author`/`signers` leaves this
/// lifecycle's actual dependency visible instead of buried in a parameter
/// list. Same shape as the root's own [`super::EngineWiring`].
#[derive(Clone, Copy)]
pub(super) struct CompletionWiring<'a> {
    pub(super) runtime: &'a tokio::runtime::Handle,
    pub(super) inbox: &'a Sender<Cmd>,
}

/// Every sign-only operation the engine currently owns, plus the id space they
/// are named in.
///
/// This is the whole lifecycle's state. Admission mints the id, installs the
/// terminal, and hands the signing wait to the adapter runtime; `cancel`,
/// `finish` and `exempt_from_shutdown_drain` are the three ways an operation
/// leaves; `cancel_for_shutdown`/`drain_for_shutdown`/`is_drained` are what
/// shutdown needs and all it needs.
pub(super) struct ActiveSignEvents {
    next_id: u64,
    live: HashMap<u64, ActiveSignEvent>,
}

impl Default for ActiveSignEvents {
    fn default() -> Self {
        Self {
            next_id: 1,
            live: HashMap::new(),
        }
    }
}

impl ActiveSignEvents {
    /// Admit one sign-only operation, or refuse it.
    ///
    /// `author` and `signers` are this lifecycle's ONLY inward dependency and
    /// are named separately on purpose: the current account is the session's
    /// state, the signing capability is the signer registry's, and this owner
    /// needs both without owning either.
    pub(super) fn admit(
        &mut self,
        author: Option<PublicKey>,
        signers: &SignerRegistry,
        wiring: CompletionWiring<'_>,
        unsigned: UnsignedEvent,
        completion: SignEventCompletion,
        reply: &Sender<Result<SignEventRegistration, SignEventError>>,
    ) {
        let CompletionWiring {
            runtime: runtime_handle,
            inbox,
        } = wiring;
        let Some(author) = author else {
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
        let Some(signer_op) = signers.sign(unsigned.clone()) else {
            let _ = reply.send(Err(SignEventError::NoCurrentSigningProvider));
            return;
        };

        let operation_id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);

        // #704: the SIGNING WAIT holds no thread. A ready local signer has its
        // result now; a pending remote signer is awaited by an async task on
        // the adapter runtime. Cancellation fires the pending op's canceller (a
        // no-op for a ready result); the foreign `completion` — which may block
        // and may call `Engine::join()` reentrantly — always runs on a FRESH
        // per-op OS thread, never the runtime or the reducer.
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

        self.live.insert(
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
            self.live.remove(&operation_id);
            terminal.cancel();
            return;
        }

        let inbox = inbox.clone();
        match signer_source {
            SignEventSignerResult::Ready(result) => {
                spawn_completion(
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
                // The signing wait is async; the (possibly-blocking) foreign
                // completion is delivered on a per-op thread whether the await
                // resolves OR the task's future is dropped at runtime shutdown
                // (the dispatch Drop guard).
                let dispatch = CompletionDispatch {
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

    /// The caller cancelled, or the engine is closing this one operation:
    /// claim the terminal so a racing completion cannot also claim it.
    pub(super) fn cancel(&mut self, id: u64) {
        if let Some(active) = self.live.remove(&id) {
            active.terminal.cancel();
        }
    }

    /// The completion thread ran to the end (panic-safe: posted by a drop
    /// guard). The terminal was already claimed by whoever won.
    pub(super) fn finish(&mut self, id: u64) {
        self.live.remove(&id);
    }

    /// A completion closure is calling `Engine::join()` reentrantly from its
    /// own operation's thread. Shutdown must not wait for the operation that
    /// is itself waiting for shutdown, so exactly that one leaves the drain —
    /// without claiming its terminal, because its completion is still running.
    pub(super) fn exempt_from_shutdown_drain(&mut self, id: u64) {
        self.live.remove(&id);
    }

    /// Shutdown began: claim every terminal so no operation can still resolve,
    /// but keep the entries — the drain is not finished until each completion
    /// thread reports back.
    pub(super) fn cancel_for_shutdown(&self) {
        for active in self.live.values() {
            active.terminal.cancel();
        }
    }

    /// Last resort at loop exit: nothing will report back now, so claim and
    /// forget.
    pub(super) fn drain_for_shutdown(&mut self) {
        for (_, active) in self.live.drain() {
            active.terminal.cancel();
        }
    }

    /// No operation of this lifecycle can still run foreign code.
    pub(super) fn is_drained(&self) -> bool {
        self.live.is_empty()
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
fn spawn_completion(
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
                let _finished = FinishedGuard {
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
struct CompletionDispatch {
    inbox: Sender<Cmd>,
    operation_id: u64,
    terminal: Arc<SignEventTerminal>,
    unsigned: UnsignedEvent,
    expected_id: EventId,
    completion: Option<SignEventCompletion>,
    signer_result: Option<Result<SignerSignedEvent, nmp_signer::SignerError>>,
}

impl Drop for CompletionDispatch {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            spawn_completion(
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

/// Posts `SignEventFinished` however the completion thread ends, including a
/// panic inside the foreign closure. Without it a panicking completion would
/// leave its operation in the drain forever and shutdown would never finish.
struct FinishedGuard {
    inbox: Sender<Cmd>,
    operation_id: u64,
}

impl Drop for FinishedGuard {
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

/// The facade verbs for this lifecycle. They live with the owner because the
/// registration they return is the owner's own state; `Handle` itself keeps
/// only the inbox they send on.
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

/// #1628: these drive [`ActiveSignEvents`] DIRECTLY — no `EngineCore`, no
/// `engine_loop`, no `Engine`, no relay, no store. That is the ownership
/// claim, stated as a test: if any of this lifecycle still needed root
/// coordination to be exercised, none of these could be written.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::AuthCapabilityInstance;
    use crate::runtime::SignerRegistry;
    use nostr::{Keys, Kind, Tag, Timestamp};
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);

    fn adapter_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test adapter runtime")
    }

    fn unsigned_for(keys: &Keys) -> UnsignedEvent {
        UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(1_700_000_000),
            Kind::from(1u16),
            Vec::<Tag>::new(),
            "sign me".to_string(),
        )
    }

    fn registry_with(keys: &Keys) -> SignerRegistry {
        let mut registry = SignerRegistry::default();
        let signer = nmp_local_signer::LocalKeySigner::from_secret_bytes(
            keys.secret_key().as_secret_bytes(),
        )
        .expect("fixture key is a valid secp256k1 scalar");
        registry.add_local(keys.public_key(), AuthCapabilityInstance(1), signer);
        registry
    }

    /// Every refusal path leaves NOTHING admitted. A refused request that
    /// still occupied the drain would hang shutdown forever.
    #[test]
    fn a_refused_request_admits_no_operation() {
        let runtime = adapter_runtime();
        let keys = Keys::generate();
        let other = Keys::generate();
        let (inbox, _inbox_rx) = mpsc::channel();

        let refusals: Vec<(Option<PublicKey>, SignerRegistry, UnsignedEvent, &str)> = vec![
            (
                None,
                registry_with(&keys),
                unsigned_for(&keys),
                "no current account",
            ),
            (
                Some(other.public_key()),
                registry_with(&keys),
                unsigned_for(&keys),
                "author is not the current account",
            ),
            (
                Some(keys.public_key()),
                SignerRegistry::default(),
                unsigned_for(&keys),
                "current account has no signer",
            ),
        ];

        let mut owner = ActiveSignEvents::default();
        for (author, signers, unsigned, what) in refusals {
            let (reply, reply_rx) = mpsc::channel();
            owner.admit(
                author,
                &signers,
                CompletionWiring {
                    runtime: runtime.handle(),
                    inbox: &inbox,
                },
                unsigned,
                Box::new(|_| panic!("a refused request must never reach a completion")),
                &reply,
            );
            assert!(
                reply_rx.recv().expect("refusal is replied to").is_err(),
                "{what} must be refused"
            );
            assert!(owner.is_drained(), "{what} must leave nothing in the drain");
        }
    }

    /// A body whose declared id contradicts its own contents is refused before
    /// any signer is consulted.
    #[test]
    fn a_declared_id_that_contradicts_the_body_is_refused_before_signing() {
        let runtime = adapter_runtime();
        let keys = Keys::generate();
        let (inbox, _inbox_rx) = mpsc::channel();
        let mut unsigned = unsigned_for(&keys);
        unsigned.id = Some(EventId::all_zeros());

        let mut owner = ActiveSignEvents::default();
        let (reply, reply_rx) = mpsc::channel();
        owner.admit(
            Some(keys.public_key()),
            &registry_with(&keys),
            CompletionWiring {
                runtime: runtime.handle(),
                inbox: &inbox,
            },
            unsigned,
            Box::new(|_| panic!("a refused request must never reach a completion")),
            &reply,
        );

        assert!(matches!(
            reply_rx.recv().expect("refusal is replied to"),
            Err(SignEventError::InvalidRequest { .. })
        ));
        assert!(owner.is_drained());
    }

    /// The happy path, end to end, through the owner alone: one admitted
    /// operation, one signed event delivered to the foreign completion, and
    /// one `SignEventFinished` posted so the drain can close.
    #[test]
    fn one_admitted_operation_signs_once_and_reports_finished() {
        let runtime = adapter_runtime();
        let keys = Keys::generate();
        let (inbox, inbox_rx) = mpsc::channel();
        let (signed_tx, signed_rx) = mpsc::channel();

        let mut owner = ActiveSignEvents::default();
        let (reply, reply_rx) = mpsc::channel();
        owner.admit(
            Some(keys.public_key()),
            &registry_with(&keys),
            CompletionWiring {
                runtime: runtime.handle(),
                inbox: &inbox,
            },
            unsigned_for(&keys),
            Box::new(move |result| {
                let _ = signed_tx.send(result);
            }),
            &reply,
        );
        let registration = reply_rx
            .recv()
            .expect("admission is replied to")
            .expect("a signable request is admitted");
        assert!(
            !owner.is_drained(),
            "an admitted operation holds the drain open"
        );

        let signed = signed_rx
            .recv_timeout(WAIT)
            .expect("the completion runs")
            .expect("the local signer signs");
        assert_eq!(signed.pubkey, keys.public_key());
        assert!(signed.verify().is_ok());

        match inbox_rx.recv_timeout(WAIT).expect("finished is posted") {
            Cmd::SignEventFinished(id) => assert_eq!(id, registration.id),
            other => panic!("expected SignEventFinished, got {:?}", CmdName(&other)),
        }
        owner.finish(registration.id);
        assert!(owner.is_drained());
    }

    /// A completion closure that PANICS must still report finished, or the
    /// shutdown drain never closes (#1628 falsifier 2). The panic-safe handoff
    /// is a `Drop` guard, so this holds without the owner observing the panic.
    #[test]
    fn a_panicking_completion_still_reports_finished() {
        let runtime = adapter_runtime();
        let keys = Keys::generate();
        let (inbox, inbox_rx) = mpsc::channel();

        let mut owner = ActiveSignEvents::default();
        let (reply, reply_rx) = mpsc::channel();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        owner.admit(
            Some(keys.public_key()),
            &registry_with(&keys),
            CompletionWiring {
                runtime: runtime.handle(),
                inbox: &inbox,
            },
            unsigned_for(&keys),
            Box::new(|_| panic!("foreign completion panics")),
            &reply,
        );
        let registration = reply_rx
            .recv()
            .expect("admission is replied to")
            .expect("a signable request is admitted");

        let posted = inbox_rx.recv_timeout(WAIT);
        std::panic::set_hook(previous);
        match posted.expect("finished is posted even after a panic") {
            Cmd::SignEventFinished(id) => assert_eq!(id, registration.id),
            other => panic!("expected SignEventFinished, got {:?}", CmdName(&other)),
        }
        owner.finish(registration.id);
        assert!(owner.is_drained());
    }

    /// The terminal is claimed exactly once. Cancellation and completion race
    /// for it, and the loser must not also fire the bound cancel action
    /// (#1628 falsifier 1).
    #[test]
    fn the_terminal_is_claimed_exactly_once_under_a_cancel_resolve_race() {
        let fired = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&fired);
        let terminal = SignEventTerminal::new(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));

        assert!(terminal.cancel(), "the first cancel claims the terminal");
        assert!(!terminal.cancel(), "a second cancel claims nothing");
        assert!(
            !terminal.resolve(),
            "a completion racing a cancel must not also claim the terminal"
        );
        assert_eq!(
            fired.load(Ordering::SeqCst),
            1,
            "the bound cancel action fires exactly once"
        );

        let resolved = SignEventTerminal::new(Box::new(|| {
            panic!("resolving must not fire the cancel action")
        }));
        assert!(resolved.resolve(), "the first resolve claims the terminal");
        assert!(!resolved.cancel(), "a later cancel claims nothing");
    }

    /// Shutdown accounting, which is the whole reason this owner answers
    /// `is_drained` rather than exposing a map. `cancel_for_shutdown` claims
    /// the terminals but KEEPS the entries — the drain is not finished until
    /// each completion thread reports back — while an operation exempted by a
    /// reentrant `join()` leaves immediately.
    #[test]
    fn shutdown_keeps_cancelled_operations_in_the_drain_until_they_report() {
        let runtime = adapter_runtime();
        let keys = Keys::generate();
        let (inbox, inbox_rx) = mpsc::channel();
        let (blocked_tx, blocked_rx) = mpsc::channel();

        let mut owner = ActiveSignEvents::default();
        let (reply, reply_rx) = mpsc::channel();
        owner.admit(
            Some(keys.public_key()),
            &registry_with(&keys),
            CompletionWiring {
                runtime: runtime.handle(),
                inbox: &inbox,
            },
            unsigned_for(&keys),
            Box::new(move |_| {
                // Hold the completion thread open so the drain has something
                // real to wait for.
                let _ = blocked_rx.recv_timeout(WAIT);
            }),
            &reply,
        );
        let registration = reply_rx
            .recv()
            .expect("admission is replied to")
            .expect("a signable request is admitted");

        owner.cancel_for_shutdown();
        assert!(
            !owner.is_drained(),
            "a cancelled operation whose completion is still running keeps the drain open"
        );

        let _ = blocked_tx.send(());
        match inbox_rx.recv_timeout(WAIT).expect("finished is posted") {
            Cmd::SignEventFinished(id) => assert_eq!(id, registration.id),
            other => panic!("expected SignEventFinished, got {:?}", CmdName(&other)),
        }
        owner.finish(registration.id);
        assert!(owner.is_drained(), "reporting back closes the drain");
    }

    /// A reentrant `Engine::join()` from inside a completion exempts exactly
    /// its own operation, so shutdown does not wait for the operation that is
    /// itself waiting for shutdown.
    #[test]
    fn an_exempted_operation_leaves_the_drain_without_reporting() {
        let runtime = adapter_runtime();
        let keys = Keys::generate();
        let (inbox, inbox_rx) = mpsc::channel();
        let (blocked_tx, blocked_rx) = mpsc::channel();

        let mut owner = ActiveSignEvents::default();
        let (reply, reply_rx) = mpsc::channel();
        owner.admit(
            Some(keys.public_key()),
            &registry_with(&keys),
            CompletionWiring {
                runtime: runtime.handle(),
                inbox: &inbox,
            },
            unsigned_for(&keys),
            Box::new(move |_| {
                let _ = blocked_rx.recv_timeout(WAIT);
            }),
            &reply,
        );
        let registration = reply_rx
            .recv()
            .expect("admission is replied to")
            .expect("a signable request is admitted");

        assert!(!owner.is_drained());
        owner.exempt_from_shutdown_drain(registration.id);
        assert!(
            owner.is_drained(),
            "the operation calling join() reentrantly must not wait for itself"
        );

        // It still reports back when its completion ends; that report is now a
        // harmless no-op rather than the thing the drain was waiting for.
        let _ = blocked_tx.send(());
        assert!(matches!(
            inbox_rx.recv_timeout(WAIT),
            Ok(Cmd::SignEventFinished(_)) | Err(RecvTimeoutError::Timeout)
        ));
        owner.finish(registration.id);
        assert!(owner.is_drained());
    }

    /// `drain_for_shutdown` is the last resort at loop exit: nothing will
    /// report back after it, so it claims and forgets.
    #[test]
    fn draining_at_loop_exit_claims_and_forgets_every_operation() {
        let runtime = adapter_runtime();
        let keys = Keys::generate();
        let (inbox, _inbox_rx) = mpsc::channel();

        let mut owner = ActiveSignEvents::default();
        for _ in 0..3 {
            let (reply, reply_rx) = mpsc::channel();
            owner.admit(
                Some(keys.public_key()),
                &registry_with(&keys),
                CompletionWiring {
                    runtime: runtime.handle(),
                    inbox: &inbox,
                },
                unsigned_for(&keys),
                Box::new(|_| {}),
                &reply,
            );
            reply_rx
                .recv()
                .expect("admission is replied to")
                .expect("a signable request is admitted");
        }
        assert!(!owner.is_drained());
        owner.drain_for_shutdown();
        assert!(owner.is_drained());
    }

    /// Operation ids are this owner's own space and never repeat.
    #[test]
    fn operation_ids_are_distinct_and_owned_here() {
        let runtime = adapter_runtime();
        let keys = Keys::generate();
        let (inbox, _inbox_rx) = mpsc::channel();

        let mut owner = ActiveSignEvents::default();
        let mut ids = Vec::new();
        for _ in 0..4 {
            let (reply, reply_rx) = mpsc::channel();
            owner.admit(
                Some(keys.public_key()),
                &registry_with(&keys),
                CompletionWiring {
                    runtime: runtime.handle(),
                    inbox: &inbox,
                },
                unsigned_for(&keys),
                Box::new(|_| {}),
                &reply,
            );
            ids.push(
                reply_rx
                    .recv()
                    .expect("admission is replied to")
                    .expect("a signable request is admitted")
                    .id,
            );
        }
        let distinct: std::collections::BTreeSet<_> = ids.iter().copied().collect();
        assert_eq!(distinct.len(), ids.len(), "ids: {ids:?}");
        owner.drain_for_shutdown();
    }

    /// `Cmd` has no `Debug`; name the variant for assertion messages without
    /// giving the whole command vocabulary one.
    struct CmdName<'a>(&'a Cmd);

    impl std::fmt::Debug for CmdName<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.0 {
                Cmd::SignEventFinished(id) => write!(f, "SignEventFinished({id})"),
                Cmd::CancelSignEvent(id) => write!(f, "CancelSignEvent({id})"),
                _ => f.write_str("<other Cmd>"),
            }
        }
    }
}
