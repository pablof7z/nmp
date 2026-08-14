use std::sync::{Arc, Mutex};

#[cfg(doc)]
use super::NmpEngine;
use crate::convert::{frame_to_ffi, FfiRequestRowsError, FfiRowPullError};
use crate::types::FfiFrame;

/// The app-facing pull-based handle to a live subscription (returned by
/// [`NmpEngine::observe`], #680/#762). Native SDKs synchronously call
/// [`Self::begin_next`] before awaiting [`NmpRowPull::receive`], then
/// synchronously commit or abort that ticket. The ticket is private transport
/// ownership inside the existing Swift `AsyncSequence` / Kotlin `Flow`; it is
/// not another app-facing observation noun.
///
/// For unbounded delta observations, one frame may be retained in the active
/// ticket until foreign completion acknowledges it. Reducer output produced
/// meanwhile still composes in the existing one-slot engine mailbox. The
/// maximum is therefore one claimed delta plus one composed successor, never
/// one item per cancellation. Windowed frames remain self-contained snapshots
/// and are not retained on abort.
#[derive(uniffi::Object)]
pub struct NmpRowStream {
    shared: Arc<RowStreamShared>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowDeliveryMode {
    Delta,
    Snapshot,
}

struct RowStreamShared {
    inner: nmp::AsyncSubscription,
    mode: RowDeliveryMode,
    lifecycle: Mutex<RowStreamLifecycle>,
}

struct RowStreamLifecycle {
    state: RowStreamState,
}

enum RowStreamState {
    Open(Box<RowStreamOpen>),
    Closed,
}

struct RowStreamOpen {
    active: Option<ActiveRowPull>,
    retained_delta: Option<nmp::Frame>,
}

struct PullIdentity;

struct ActiveRowPull {
    identity: Arc<PullIdentity>,
    phase: RowPullPhase,
}

enum RowPullPhase {
    Fresh,
    FreshDelta(nmp::Frame),
    Awaiting,
    AbortRequested,
    AwaitFinished,
    ReadyDelta(nmp::Frame),
    ReadySnapshot,
    Terminal,
}

enum ReceiveStart {
    Retained(nmp::Frame),
    Await,
}

impl NmpRowStream {
    pub(super) fn new(inner: nmp::AsyncSubscription, windowed: bool) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(RowStreamShared {
                inner,
                mode: if windowed {
                    RowDeliveryMode::Snapshot
                } else {
                    RowDeliveryMode::Delta
                },
                lifecycle: Mutex::new(RowStreamLifecycle {
                    state: RowStreamState::Open(Box::new(RowStreamOpen {
                        active: None,
                        retained_delta: None,
                    })),
                }),
            }),
        })
    }
}

#[uniffi::export]
impl NmpRowStream {
    /// Claim the stream synchronously, before entering UniFFI's cancellable
    /// async READY/complete split. A second live ticket is refused; it never
    /// observes or replays the first ticket's retained delta.
    pub fn begin_next(&self) -> Result<Arc<NmpRowPull>, FfiRowPullError> {
        self.shared.begin_next()
    }

    /// Withdraw the subscription now, rather than waiting for `Drop` (a Swift
    /// `deinit` can be delayed by ARC in ways an app may want to preempt).
    /// Wakes any parked ticket `receive()` to `None`. Safe to call more than once, and
    /// safe to never call at all.
    pub fn cancel(&self) {
        self.shared.cancel();
    }

    /// Windowed observations only: monotonically raise the window's row
    /// target to at least `at_least`, clamped to the declared `max`.
    /// Idempotent and declarative -- calling with a value at or below the
    /// current target is a no-op; there is no continuation token to thread
    /// back and no generation to go stale (#485 replaced the opaque
    /// continuation entirely). Growth outcomes arrive as
    /// [`crate::types::FfiWindowLoad`] facts in delivered frames -- reaching
    /// the declared `max` is the `AtBound` FACT there, never an error here.
    /// Unbounded observations fail with
    /// [`FfiRequestRowsError::Unwindowed`].
    pub fn request_rows(&self, at_least: u64) -> Result<(), FfiRequestRowsError> {
        // Saturating u64→usize: `at_least` is a declarative lower bound the
        // engine clamps to the window's `max` anyway, so a value beyond the
        // platform's addressable row count is behaviorally identical to
        // usize::MAX (only reachable on sub-64-bit targets).
        let at_least = usize::try_from(at_least).unwrap_or(usize::MAX);
        self.shared
            .inner
            .request_rows(at_least)
            .map_err(FfiRequestRowsError::from)
    }
}

impl Drop for NmpRowStream {
    fn drop(&mut self) {
        self.shared.cancel();
    }
}

/// One private foreign-delivery claim for [`NmpRowStream`] (#762).
///
/// The native wrapper owns this object before it awaits [`Self::receive`].
/// After a non-cancelled return it synchronously calls [`Self::commit`];
/// every other path calls [`Self::abort`]. Dropping a ticket aborts it
/// idempotently.
#[derive(uniffi::Object)]
pub struct NmpRowPull {
    shared: Arc<RowStreamShared>,
    identity: Arc<PullIdentity>,
}

impl std::fmt::Debug for NmpRowPull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NmpRowPull").finish_non_exhaustive()
    }
}

#[uniffi::export]
impl NmpRowPull {
    /// Await this ticket's frame. A ticket is start-once: a second call is a
    /// typed refusal both while the first is pending and after it reached
    /// READY.
    pub async fn receive(&self) -> Result<Option<FfiFrame>, FfiRowPullError> {
        match self.shared.start_receive(&self.identity)? {
            ReceiveStart::Retained(frame) => Ok(Some(frame_to_ffi(frame))),
            ReceiveStart::Await => {
                let guard = ReceiveGuard::new(self.shared.clone(), self.identity.clone());
                let frame = self
                    .shared
                    .inner
                    .next()
                    .await
                    .map_err(|_| FfiRowPullError::ConcurrentNext)?;
                guard.finish(frame).map(|frame| frame.map(frame_to_ffi))
            }
        }
    }

    /// Acknowledge that foreign code obtained the result. For an unbounded
    /// frame this destructively releases the retained delta exactly once.
    pub fn commit(&self) -> Result<(), FfiRowPullError> {
        self.shared.commit(&self.identity)
    }

    /// Roll back a ticket that did not reach foreign completion. A retained
    /// unbounded delta becomes the next ticket's candidate; a windowed
    /// snapshot is self-contained and needs no replay.
    pub fn abort(&self) {
        self.shared.abort(&self.identity);
    }
}

impl Drop for NmpRowPull {
    fn drop(&mut self) {
        self.shared.abort(&self.identity);
    }
}

impl RowStreamShared {
    fn begin_next(self: &Arc<Self>) -> Result<Arc<NmpRowPull>, FfiRowPullError> {
        let identity = Arc::new(PullIdentity);
        let mut lifecycle = self.lifecycle.lock().unwrap();
        match &mut lifecycle.state {
            RowStreamState::Closed => Err(FfiRowPullError::Closed),
            RowStreamState::Open(open) => {
                if open.active.is_some() {
                    return Err(FfiRowPullError::ConcurrentNext);
                }
                let phase = open
                    .retained_delta
                    .take()
                    .map(RowPullPhase::FreshDelta)
                    .unwrap_or(RowPullPhase::Fresh);
                open.active = Some(ActiveRowPull {
                    identity: identity.clone(),
                    phase,
                });
                Ok(Arc::new(NmpRowPull {
                    shared: self.clone(),
                    identity,
                }))
            }
        }
    }

    fn start_receive(&self, identity: &Arc<PullIdentity>) -> Result<ReceiveStart, FfiRowPullError> {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        let RowStreamState::Open(open) = &mut lifecycle.state else {
            return Err(FfiRowPullError::Closed);
        };
        let Some(pull) = open.active.as_mut() else {
            return Err(FfiRowPullError::Finished);
        };
        if !Arc::ptr_eq(&pull.identity, identity) {
            return Err(FfiRowPullError::Finished);
        }
        match std::mem::replace(&mut pull.phase, RowPullPhase::AwaitFinished) {
            RowPullPhase::Fresh => {
                pull.phase = RowPullPhase::Awaiting;
                Ok(ReceiveStart::Await)
            }
            RowPullPhase::FreshDelta(frame) => {
                let returned = frame.clone();
                pull.phase = RowPullPhase::ReadyDelta(frame);
                Ok(ReceiveStart::Retained(returned))
            }
            phase => {
                pull.phase = phase;
                Err(FfiRowPullError::ReceiveAlreadyStarted)
            }
        }
    }

    fn finish_receive(
        &self,
        identity: &Arc<PullIdentity>,
        frame: Option<nmp::Frame>,
    ) -> Result<Option<nmp::Frame>, FfiRowPullError> {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        let RowStreamState::Open(open) = &mut lifecycle.state else {
            return if frame.is_none() {
                Ok(None)
            } else {
                Err(FfiRowPullError::Closed)
            };
        };
        let Some(mut pull) = open.active.take() else {
            return Err(FfiRowPullError::Finished);
        };
        if !Arc::ptr_eq(&pull.identity, identity) {
            open.active = Some(pull);
            return Err(FfiRowPullError::Finished);
        }
        match &pull.phase {
            RowPullPhase::Awaiting => {}
            RowPullPhase::AbortRequested => {
                if self.mode == RowDeliveryMode::Delta {
                    open.retained_delta = frame;
                }
                return Err(FfiRowPullError::Aborted);
            }
            _ => {
                open.active = Some(pull);
                return Err(FfiRowPullError::Finished);
            }
        }

        match frame {
            Some(frame) if self.mode == RowDeliveryMode::Delta => {
                let returned = frame.clone();
                pull.phase = RowPullPhase::ReadyDelta(frame);
                open.active = Some(pull);
                Ok(Some(returned))
            }
            Some(frame) => {
                pull.phase = RowPullPhase::ReadySnapshot;
                open.active = Some(pull);
                Ok(Some(frame))
            }
            None => {
                pull.phase = RowPullPhase::Terminal;
                open.active = Some(pull);
                Ok(None)
            }
        }
    }

    fn commit(&self, identity: &Arc<PullIdentity>) -> Result<(), FfiRowPullError> {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        let RowStreamState::Open(open) = &mut lifecycle.state else {
            return Err(FfiRowPullError::Closed);
        };
        let Some(pull) = open.active.as_ref() else {
            return Err(FfiRowPullError::Finished);
        };
        if !Arc::ptr_eq(&pull.identity, identity) {
            return Err(FfiRowPullError::Finished);
        }
        match pull.phase {
            RowPullPhase::ReadyDelta(_) | RowPullPhase::ReadySnapshot | RowPullPhase::Terminal => {
                open.active = None;
                Ok(())
            }
            RowPullPhase::Fresh
            | RowPullPhase::FreshDelta(_)
            | RowPullPhase::Awaiting
            | RowPullPhase::AbortRequested
            | RowPullPhase::AwaitFinished => Err(FfiRowPullError::NotReady),
        }
    }

    fn abort(&self, identity: &Arc<PullIdentity>) {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        let RowStreamState::Open(open) = &mut lifecycle.state else {
            return;
        };
        let Some(mut pull) = open.active.take() else {
            return;
        };
        if !Arc::ptr_eq(&pull.identity, identity) {
            open.active = Some(pull);
            return;
        }
        match pull.phase {
            RowPullPhase::FreshDelta(frame) | RowPullPhase::ReadyDelta(frame) => {
                open.retained_delta = Some(frame);
            }
            RowPullPhase::Awaiting => {
                pull.phase = RowPullPhase::AbortRequested;
                open.active = Some(pull);
            }
            RowPullPhase::Fresh
            | RowPullPhase::AbortRequested
            | RowPullPhase::AwaitFinished
            | RowPullPhase::ReadySnapshot
            | RowPullPhase::Terminal => {}
        }
    }

    fn receive_dropped(&self, identity: &Arc<PullIdentity>) {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        let RowStreamState::Open(open) = &mut lifecycle.state else {
            return;
        };
        let Some(mut pull) = open.active.take() else {
            return;
        };
        if !Arc::ptr_eq(&pull.identity, identity) {
            open.active = Some(pull);
            return;
        }
        match pull.phase {
            RowPullPhase::AbortRequested => {}
            RowPullPhase::Awaiting => {
                pull.phase = RowPullPhase::AwaitFinished;
                open.active = Some(pull);
            }
            _ => {
                open.active = Some(pull);
            }
        }
    }

    fn cancel(&self) {
        let should_cancel = {
            let mut lifecycle = self.lifecycle.lock().unwrap();
            if matches!(lifecycle.state, RowStreamState::Closed) {
                false
            } else {
                lifecycle.state = RowStreamState::Closed;
                true
            }
        };
        if should_cancel {
            self.inner.cancel();
        }
    }
}

struct ReceiveGuard {
    pending: Option<(Arc<RowStreamShared>, Arc<PullIdentity>)>,
}

impl ReceiveGuard {
    fn new(shared: Arc<RowStreamShared>, identity: Arc<PullIdentity>) -> Self {
        Self {
            pending: Some((shared, identity)),
        }
    }

    fn finish(mut self, frame: Option<nmp::Frame>) -> Result<Option<nmp::Frame>, FfiRowPullError> {
        let (shared, identity) = self.pending.take().expect("receive guard is armed");
        shared.finish_receive(&identity, frame)
    }
}

impl Drop for ReceiveGuard {
    fn drop(&mut self) {
        if let Some((shared, identity)) = self.pending.take() {
            shared.receive_dropped(&identity);
        }
    }
}
