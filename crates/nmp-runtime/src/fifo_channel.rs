//! A waker-aware FIFO fact channel (#680).
//!
//! Unlike the latest-wins single-slot mailbox (`diagnostics_channel.rs`), this
//! preserves **every** value in order. Receipt (`WriteFact`) and
//! follow-action-status transitions are per-lane facts where a later value does
//! not subsume an earlier one (`Sent{relay}`, `AwaitingAuth`, per-attempt
//! ordinals …), so they must not conflate.
//!
//! Retained cardinality is bounded directly by [`FACT_CHANNEL_CAPACITY`].
//! Durable retry has no attempt-count ceiling, so lifecycle cardinality cannot
//! be used as a memory bound. If a consumer falls behind the finite queue, the
//! channel keeps its already-buffered prefix, rejects further sends, then
//! surfaces [`FifoNextError::Lagged`] after that prefix drains. Receipt callers
//! can reattach to the publish-queue Redb source of truth; no fact is silently
//! presented as delivered and no paused app can grow this queue without bound.
//!
//! Delivery works both ways over the same queue, mirroring the latest mailbox:
//! blocking `recv`/`recv_timeout` (with typed close/lag outcomes) and a
//! waker-aware
//! [`AsyncFifoReceiver::next`] with no blocked OS thread. Termination is the
//! same two-cause enum: producer `Drop` (`ProducerGone` — drain then end) vs
//! consumer [`FifoReceiver::close`] (`Cancelled` — end now, drop the backlog).

use std::collections::VecDeque;
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FifoState {
    Open,
    ProducerGone,
    Cancelled,
    Lagged,
}

struct Queue<T> {
    items: VecDeque<T>,
    state: FifoState,
    waker: Option<Waker>,
    close_hook: Option<Box<dyn FnOnce() + Send + 'static>>,
    /// How a newly-sent value SUPERSEDES one still queued, if this channel's
    /// facts have that relation at all. Consulted only under back-pressure:
    /// while there is room, every value is retained in order, so a consumer
    /// that keeps up still observes every intermediate state.
    supersedes: Option<fn(&T, &T) -> bool>,
}

struct Inner<T> {
    queue: Mutex<Queue<T>>,
    cvar: Condvar,
}

/// Maximum retained live facts per receipt/follow-action observer. This is an
/// internal delivery bound, not an app admission limit. Durable facts beyond
/// it remain in the store and require replay.
pub const FACT_CHANNEL_CAPACITY: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FifoRecvError {
    Closed,
    Lagged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FifoRecvTimeoutError {
    Timeout,
    Closed,
    Lagged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FifoTryRecvError {
    Empty,
    Closed,
    Lagged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FifoNextError {
    ConcurrentNext,
    Lagged,
}

/// The producer half. Dropping it ends the stream after the backlog drains.
pub struct FifoSender<T> {
    inner: Arc<Inner<T>>,
}

/// The single-consumer half. `Send` but deliberately not the concurrent
/// multi-reader shape — one drain (blocking or async) owns it.
pub struct FifoReceiver<T> {
    inner: Arc<Inner<T>>,
}

/// A fresh empty, open FIFO channel with a fixed finite live-delivery bound.
/// Nothing supersedes anything: a full queue lags, exactly as before.
pub fn fifo_channel<T>() -> (FifoSender<T>, FifoReceiver<T>) {
    channel_with(None)
}

/// A FIFO channel whose facts have a SUPERSEDING relation: `supersedes(new,
/// queued)` is true when `new` states the same thing about the same subject as
/// `queued`, so retaining both says nothing the newer one does not.
///
/// The relation is consulted ONLY when the queue is full. Below the bound this
/// behaves identically to [`fifo_channel`] and every intermediate state is
/// delivered in order — a consumer that keeps up loses nothing. At the bound,
/// a value that supersedes queued ones evicts them instead of poisoning the
/// channel: the consumer may then miss states it was too slow to read, but it
/// always converges on the current one, and a stream can no longer be killed
/// outright by a subject that keeps changing (a relay retrying, say).
///
/// The bound therefore stops being load-bearing for correctness. It is the
/// point at which delivery starts compressing, not the point at which it dies.
pub fn superseding_fifo_channel<T>(
    supersedes: fn(&T, &T) -> bool,
) -> (FifoSender<T>, FifoReceiver<T>) {
    channel_with(Some(supersedes))
}

fn channel_with<T>(supersedes: Option<fn(&T, &T) -> bool>) -> (FifoSender<T>, FifoReceiver<T>) {
    let inner = Arc::new(Inner {
        queue: Mutex::new(Queue {
            items: VecDeque::new(),
            state: FifoState::Open,
            waker: None,
            close_hook: None,
            supersedes,
        }),
        cvar: Condvar::new(),
    });
    (
        FifoSender {
            inner: inner.clone(),
        },
        FifoReceiver { inner },
    )
}

impl<T> FifoSender<T> {
    /// Append a fact and wake the receiver. Returns `false` once the consumer
    /// has cancelled or this finite queue has lagged.
    ///
    /// At the bound, a channel built by [`superseding_fifo_channel`] first
    /// tries to make room by evicting queued values this one supersedes. Only
    /// when nothing can be evicted — every queued value states something this
    /// one does not — is the prefix retained and the channel lagged, leaving
    /// the rejected value to its durable owner rather than claiming it live.
    pub fn send(&self, value: T) -> bool {
        let (accepted, waker) = {
            let mut queue = self.inner.queue.lock().unwrap();
            if queue.state != FifoState::Open {
                return false;
            }
            if queue.items.len() == FACT_CHANNEL_CAPACITY {
                let evicted = match queue.supersedes {
                    Some(supersedes) => {
                        let before = queue.items.len();
                        queue.items.retain(|queued| !supersedes(&value, queued));
                        before != queue.items.len()
                    }
                    None => false,
                };
                if evicted {
                    queue.items.push_back(value);
                    self.inner.cvar.notify_one();
                    (true, queue.waker.take())
                } else {
                    queue.state = FifoState::Lagged;
                    self.inner.cvar.notify_all();
                    (false, queue.waker.take())
                }
            } else {
                queue.items.push_back(value);
                self.inner.cvar.notify_one();
                (true, queue.waker.take())
            }
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        accepted
    }
}

impl<T> Drop for FifoSender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut queue = self.inner.queue.lock().unwrap();
            if queue.state == FifoState::Open {
                queue.state = FifoState::ProducerGone;
            }
            self.inner.cvar.notify_one();
            queue.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> FifoReceiver<T> {
    /// Block for the next fact in order, `Err(RecvError)` once the producer is
    /// gone and the backlog is drained, or the consumer has closed the channel.
    /// Signature matches `std::sync::mpsc::Receiver::recv`.
    pub fn recv(&self) -> Result<T, FifoRecvError> {
        let mut queue = self.inner.queue.lock().unwrap();
        loop {
            if queue.state == FifoState::Cancelled {
                return Err(FifoRecvError::Closed);
            }
            if let Some(value) = queue.items.pop_front() {
                return Ok(value);
            }
            match queue.state {
                FifoState::ProducerGone => return Err(FifoRecvError::Closed),
                FifoState::Lagged => return Err(FifoRecvError::Lagged),
                FifoState::Open => {}
                FifoState::Cancelled => unreachable!("handled above"),
            }
            queue = self.inner.cvar.wait(queue).unwrap();
        }
    }

    /// Block at most `timeout` for the next fact. Signature matches
    /// `std::sync::mpsc::Receiver::recv_timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, FifoRecvTimeoutError> {
        let queue = self.inner.queue.lock().unwrap();
        let (mut queue, wait) = self
            .inner
            .cvar
            .wait_timeout_while(queue, timeout, |queue| {
                queue.items.is_empty() && queue.state == FifoState::Open
            })
            .unwrap();
        if queue.state == FifoState::Cancelled {
            return Err(FifoRecvTimeoutError::Closed);
        }
        if let Some(value) = queue.items.pop_front() {
            return Ok(value);
        }
        match queue.state {
            FifoState::ProducerGone => Err(FifoRecvTimeoutError::Closed),
            FifoState::Lagged => Err(FifoRecvTimeoutError::Lagged),
            FifoState::Open if wait.timed_out() => Err(FifoRecvTimeoutError::Timeout),
            FifoState::Open => {
                unreachable!("condvar wait ended without an item, close, lag, or timeout")
            }
            FifoState::Cancelled => unreachable!("handled above"),
        }
    }

    /// Return the next fact immediately if one is queued, distinguishing an
    /// empty open channel from a closed one like `mpsc::Receiver::try_recv`.
    pub fn try_recv(&self) -> Result<T, FifoTryRecvError> {
        let mut queue = self.inner.queue.lock().unwrap();
        if queue.state == FifoState::Cancelled {
            return Err(FifoTryRecvError::Closed);
        }
        if let Some(value) = queue.items.pop_front() {
            return Ok(value);
        }
        match queue.state {
            FifoState::ProducerGone => Err(FifoTryRecvError::Closed),
            FifoState::Lagged => Err(FifoTryRecvError::Lagged),
            FifoState::Open => Err(FifoTryRecvError::Empty),
            FifoState::Cancelled => unreachable!("handled above"),
        }
    }

    /// Poll for the next fact without blocking a thread; registers `cx`'s waker
    /// when the queue is empty and open. `Ready(None)` on end-of-stream.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Result<Option<T>, FifoNextError>> {
        let mut queue = self.inner.queue.lock().unwrap();
        if queue.state == FifoState::Cancelled {
            queue.waker = None;
            return Poll::Ready(Ok(None));
        }
        if let Some(value) = queue.items.pop_front() {
            queue.waker = None;
            return Poll::Ready(Ok(Some(value)));
        }
        match queue.state {
            FifoState::ProducerGone => {
                queue.waker = None;
                return Poll::Ready(Ok(None));
            }
            FifoState::Lagged => {
                queue.waker = None;
                return Poll::Ready(Err(FifoNextError::Lagged));
            }
            FifoState::Open => {}
            FifoState::Cancelled => unreachable!("handled above"),
        }
        queue.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    /// Consumer-initiated idempotent close: drops the backlog, ends the stream
    /// now, and wakes a blocked thread or parked async reader.
    pub fn close(&self) {
        let (waker, close_hook) = {
            let mut queue = self.inner.queue.lock().unwrap();
            queue.state = FifoState::Cancelled;
            queue.items.clear();
            self.inner.cvar.notify_all();
            (queue.waker.take(), queue.close_hook.take())
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        if let Some(close_hook) = close_hook {
            close_hook();
        }
    }

    /// Forget the waker one `next()` future registered, when that future is
    /// dropped — which is what a cancelled Kotlin collection does, since
    /// UniFFI frees the Rust future on the way out. A stale waker left behind
    /// would be woken instead of the reader that replaces it.
    fn end_park(&self) {
        self.inner.queue.lock().unwrap().waker = None;
    }

    /// Install one consumer-lifecycle callback. Receipt streams use this to
    /// withdraw their exact reducer-side observer on close/drop; ordinary
    /// FIFO users leave it unset.
    pub(crate) fn set_close_hook(&self, close_hook: impl FnOnce() + Send + 'static) {
        let close_hook = {
            let mut queue = self.inner.queue.lock().unwrap();
            if queue.state == FifoState::Cancelled {
                Some(Box::new(close_hook) as Box<dyn FnOnce() + Send + 'static>)
            } else {
                debug_assert!(queue.close_hook.is_none());
                queue.close_hook = Some(Box::new(close_hook));
                None
            }
        };
        if let Some(close_hook) = close_hook {
            close_hook();
        }
    }

    /// Convert to the `Send + Sync` async pull surface (#680).
    pub fn into_async(self) -> AsyncFifoReceiver<T> {
        AsyncFifoReceiver {
            rx: self,
            reading: AtomicBool::new(false),
        }
    }
}

impl<T> Drop for FifoReceiver<T> {
    fn drop(&mut self) {
        self.close();
    }
}

/// The `Send + Sync` async pull surface over a [`FifoReceiver`] (#680), with a
/// single-reader guard so exactly one `next()` future parks on the queue's one
/// waker slot.
pub struct AsyncFifoReceiver<T> {
    rx: FifoReceiver<T>,
    reading: AtomicBool,
}

impl<T> AsyncFifoReceiver<T> {
    /// Await the next fact in order, or `None` at end-of-stream.
    /// [`FifoNextError::ConcurrentNext`] on an overlapping call.
    pub async fn next(&self) -> Result<Option<T>, FifoNextError> {
        if self.reading.swap(true, Ordering::AcqRel) {
            return Err(FifoNextError::ConcurrentNext);
        }
        let _guard = ReadingGuard {
            reading: &self.reading,
            rx: &self.rx,
        };
        poll_fn(|cx| self.rx.poll_recv(cx)).await
    }

    /// Idempotent consumer-initiated close; wakes a parked `next()` to `None`.
    pub fn close(&self) {
        self.rx.close();
    }
}

struct ReadingGuard<'a, T> {
    reading: &'a AtomicBool,
    rx: &'a FifoReceiver<T>,
}

impl<T> Drop for ReadingGuard<'_, T> {
    fn drop(&mut self) {
        self.rx.end_park();
        self.reading.store(false, Ordering::Release);
    }
}

