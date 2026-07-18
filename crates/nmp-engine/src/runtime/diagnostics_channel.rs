//! A single-slot mailbox (M5 plan §1.2 step 4). Diagnostics and history use
//! it as "latest complete snapshot wins" storage; ordinary row delivery uses
//! the same slot with an atomic in-place fold that rebases skipped deltas.
//! In every case a slow consumer has at most one pending value, never a
//! growing backlog.
//!
//! The mailbox supports both delivery disciplines over the *same* slot:
//!
//! - Blocking `recv`/`recv_timeout`/`try_recv` via `Condvar::wait` (D8: never
//!   a poll loop), for direct-Rust consumers.
//! - A waker-aware [`LatestReceiver::poll_recv`] and async
//!   [`AsyncLatestReceiver::next`] (#680), so a foreign UniFFI consumer pulls
//!   the next value by awaiting a future — no dedicated OS thread blocks on a
//!   receiver. Async is additive; the blocking verbs remain the same code over
//!   the same slot, not a second queue.
//!
//! Termination has two distinct causes, kept as an enum rather than a
//! lifecycle bool (AGENTS.md gate 3):
//!
//! - [`SlotState::ProducerGone`] — the paired [`LatestSender`] was dropped
//!   (natural teardown / engine shutdown). Any value already in the slot is
//!   still delivered, *then* the stream ends — mirroring `mpsc::Receiver`.
//! - [`SlotState::Cancelled`] — the *consumer* called [`LatestReceiver::close`]
//!   (an explicit `cancel()`). The stream ends immediately with no further
//!   value: a pending latest snapshot is discarded so no post-cancel frame is
//!   ever observed (#680 cancellation contract).

use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// Why a mailbox has stopped yielding values. Distinguishes producer teardown
/// (deliver the last value, then end) from consumer cancellation (end now,
/// discard any pending value).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Open,
    ProducerGone,
    Cancelled,
}

struct Slot<T> {
    value: Option<T>,
    state: SlotState,
    /// The waker of an async reader currently parked in `poll_recv`. Taken and
    /// woken under the slot lock by any state transition; there is at most one
    /// because concurrent `next()` is structurally rejected (see
    /// [`AsyncLatestReceiver`]).
    waker: Option<Waker>,
}

struct Inner<T> {
    slot: Mutex<Slot<T>>,
    cvar: Condvar,
}

/// The producer half. Dropping it ends the mailbox (`ProducerGone`) after any
/// pending value is delivered, and wakes its receiver (blocked thread or parked
/// async reader).
pub struct LatestSender<T> {
    inner: Arc<Inner<T>>,
}

/// The consumer half shared by self-contained latest-state streams.
pub struct LatestReceiver<T> {
    inner: Arc<Inner<T>>,
}

/// A fresh single-slot mailbox pair, empty and open.
pub fn latest_channel<T>() -> (LatestSender<T>, LatestReceiver<T>) {
    let inner = Arc::new(Inner {
        slot: Mutex::new(Slot {
            value: None,
            state: SlotState::Open,
            waker: None,
        }),
        cvar: Condvar::new(),
    });
    (
        LatestSender {
            inner: inner.clone(),
        },
        LatestReceiver { inner },
    )
}

impl<T> LatestSender<T> {
    /// Overwrite the slot with `value` and wake the receiver. A value the
    /// receiver had not yet consumed is silently dropped — the whole point of
    /// "latest wins" (see module doc).
    pub fn send(&self, value: T) {
        self.update(|pending| *pending = Some(value));
    }

    /// Mutate the pending value while holding the slot lock, then wake the
    /// receiver. Row delivery uses this to compose a new reducer delta onto the
    /// single pending rebased transition without any gap in which a consumer
    /// could observe the intermediate value. A no-op once the consumer has
    /// cancelled (the value would never be read, so it is not retained).
    pub(crate) fn update(&self, update: impl FnOnce(&mut Option<T>)) {
        let waker = {
            let mut slot = self.inner.slot.lock().unwrap();
            if slot.state == SlotState::Cancelled {
                return;
            }
            update(&mut slot.value);
            self.inner.cvar.notify_one();
            slot.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> Drop for LatestSender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut slot = self.inner.slot.lock().unwrap();
            if slot.state == SlotState::Open {
                slot.state = SlotState::ProducerGone;
            }
            self.inner.cvar.notify_one();
            slot.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> LatestReceiver<T> {
    /// Block until a value is available (D8-compliant `Condvar::wait`,
    /// never a poll loop), returning `None` once the paired `LatestSender`
    /// has been dropped and no value remains unconsumed, or the consumer has
    /// [`close`](Self::close)d the mailbox — mirrors `mpsc::Receiver::recv`'s
    /// `Err` on sender disconnect.
    pub fn recv(&self) -> Option<T> {
        let mut slot = self.inner.slot.lock().unwrap();
        loop {
            if slot.state == SlotState::Cancelled {
                return None;
            }
            if let Some(value) = slot.value.take() {
                return Some(value);
            }
            if slot.state == SlotState::ProducerGone {
                return None;
            }
            slot = self.inner.cvar.wait(slot).unwrap();
        }
    }

    /// Wait at most `timeout` for the newest value. This uses the same
    /// condvar and one slot as [`Self::recv`]; it does not poll or create a
    /// second queue.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        let slot = self.inner.slot.lock().unwrap();
        let (mut slot, wait) = self
            .inner
            .cvar
            .wait_timeout_while(slot, timeout, |slot| {
                slot.value.is_none() && slot.state == SlotState::Open
            })
            .unwrap();
        if slot.state == SlotState::Cancelled {
            return Err(RecvTimeoutError::Disconnected);
        }
        if let Some(value) = slot.value.take() {
            return Ok(value);
        }
        if slot.state == SlotState::ProducerGone {
            Err(RecvTimeoutError::Disconnected)
        } else if wait.timed_out() {
            Err(RecvTimeoutError::Timeout)
        } else {
            unreachable!("condvar wait ended without a value, close, or timeout")
        }
    }

    /// Return the pending value immediately, distinguishing an empty open
    /// slot from a closed one like `mpsc::Receiver::try_recv`.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        let mut slot = self.inner.slot.lock().unwrap();
        if slot.state == SlotState::Cancelled {
            return Err(TryRecvError::Disconnected);
        }
        if let Some(value) = slot.value.take() {
            return Ok(value);
        }
        if slot.state == SlotState::ProducerGone {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    /// Poll for the next value without blocking a thread (#680). Returns
    /// `Ready(Some(v))` for the pending latest value, `Ready(None)` once the
    /// producer is gone (and no value remains) or the consumer has cancelled,
    /// and otherwise registers `cx`'s waker and returns `Pending`. The waker is
    /// woken by any later `send`/`update`, producer `Drop`, or [`close`](Self::close).
    ///
    /// This is the single-reader poll primitive; concurrent pollers would race
    /// the one waker slot, so the async surface ([`AsyncLatestReceiver`])
    /// serialises callers.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut slot = self.inner.slot.lock().unwrap();
        if slot.state == SlotState::Cancelled {
            slot.waker = None;
            return Poll::Ready(None);
        }
        if let Some(value) = slot.value.take() {
            // Clear any waker a prior `Pending` left registered: this future is
            // resolving and will not be polled again, so it must not leave a
            // stale waker holding foreign continuation plumbing alive.
            slot.waker = None;
            return Poll::Ready(Some(value));
        }
        if slot.state == SlotState::ProducerGone {
            slot.waker = None;
            return Poll::Ready(None);
        }
        // Register (or refresh) the parked reader's waker under the same lock
        // that every producer transition takes, so no wakeup can be lost
        // between this check and the next `send`/`Drop`/`close`.
        slot.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    /// Convert to the `Send + Sync` async pull surface (#680). Consumes the
    /// blocking receiver — a latest-state stream is drained either by a
    /// direct-Rust blocking consumer or an async foreign consumer, never both.
    pub fn into_async(self) -> AsyncLatestReceiver<T> {
        AsyncLatestReceiver::new(self)
    }

    /// Consumer-initiated idempotent close (an explicit `cancel()`). Ends the
    /// stream *now*: discards any pending value, transitions to `Cancelled`,
    /// and wakes a blocked thread or parked async reader so a pending
    /// `recv`/`next` returns `None` immediately. Safe to race the producer's
    /// `Drop` and an in-flight `send` — the one slot lock orders them.
    pub fn close(&self) {
        let waker = {
            let mut slot = self.inner.slot.lock().unwrap();
            slot.state = SlotState::Cancelled;
            slot.value = None;
            self.inner.cvar.notify_all();
            slot.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// A misuse error: a second `next()` was started while one was already
/// in flight on the same async receiver (#680). The pull streams are
/// single-consumer by construction (one `AsyncSequence`/`Flow` iterates a
/// handle); overlapping `next()` calls would race the mailbox's single parked
/// waker, so they are rejected rather than silently dropping a wakeup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConcurrentNext;

impl std::fmt::Display for ConcurrentNext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a concurrent next() is already in flight on this observation handle")
    }
}

impl std::error::Error for ConcurrentNext {}

/// The `Send + Sync` async pull surface over a [`LatestReceiver`] (#680). Holds
/// the receiver plus a single-reader guard so exactly one `next()` future is
/// ever parked on the mailbox's one waker slot. Cloneable state is not
/// exposed; a handle owns exactly one of these.
pub struct AsyncLatestReceiver<T> {
    rx: LatestReceiver<T>,
    reading: AtomicBool,
}

impl<T> AsyncLatestReceiver<T> {
    pub(crate) fn new(rx: LatestReceiver<T>) -> Self {
        Self {
            rx,
            reading: AtomicBool::new(false),
        }
    }

    /// Await the next value: `Some(v)` for the latest pending value, `None`
    /// once the producer is gone or the consumer cancelled. Rejects a
    /// concurrent overlapping call with [`ConcurrentNext`]. No thread blocks;
    /// the future is woken by a `send`/producer `Drop`/`close`.
    pub async fn next(&self) -> Result<Option<T>, ConcurrentNext> {
        if self.reading.swap(true, Ordering::AcqRel) {
            return Err(ConcurrentNext);
        }
        let _guard = ReadingGuard(&self.reading);
        let value = poll_fn(|cx| self.rx.poll_recv(cx)).await;
        Ok(value)
    }

    /// Idempotent consumer-initiated close; wakes a parked `next()` to `None`.
    pub fn close(&self) {
        self.rx.close();
    }
}

/// Clears the single-reader flag when a `next()` future completes or is
/// dropped mid-poll (foreign task cancellation), so the next call can proceed.
struct ReadingGuard<'a>(&'a AtomicBool);

impl Drop for ReadingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn recv_blocks_until_a_value_is_sent() {
        let (tx, rx) = latest_channel::<u32>();
        let handle = thread::spawn(move || rx.recv());
        thread::sleep(Duration::from_millis(20));
        tx.send(7);
        assert_eq!(handle.join().unwrap(), Some(7));
    }

    #[test]
    fn only_the_latest_value_is_ever_delivered() {
        let (tx, rx) = latest_channel::<u32>();
        tx.send(1);
        tx.send(2);
        tx.send(3);
        assert_eq!(rx.recv(), Some(3));
    }

    #[test]
    fn dropping_the_sender_unblocks_recv_with_none() {
        let (tx, rx) = latest_channel::<u32>();
        let handle = thread::spawn(move || rx.recv());
        thread::sleep(Duration::from_millis(20));
        drop(tx);
        assert_eq!(handle.join().unwrap(), None);
    }

    #[test]
    fn timeout_distinguishes_elapsed_wait_from_disconnect() {
        let (tx, rx) = latest_channel::<u32>();
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(1)),
            Err(RecvTimeoutError::Timeout)
        );
        drop(tx);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn try_recv_distinguishes_empty_value_and_disconnect() {
        let (tx, rx) = latest_channel::<u32>();
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        tx.send(9);
        assert_eq!(rx.try_recv(), Ok(9));
        drop(tx);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    }

    // ---- #680: waker-aware async pull (no thread blocks on the receiver) ----

    use std::future::Future;
    use std::pin::pin;
    use std::task::Waker;

    /// A `send` after an async `next()` has parked must wake it — the whole
    /// point of the primitive. `#[tokio::test]` drives a real multi-thread
    /// executor so the wakeup is genuine, not a re-poll.
    #[tokio::test]
    async fn async_next_wakes_on_send() {
        let (tx, rx) = latest_channel::<u32>();
        let rx = AsyncLatestReceiver::new(rx);
        let waiter = tokio::spawn(async move { rx.next().await });
        // Give the waiter time to park on the mailbox before the value lands.
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(42);
        assert_eq!(waiter.await.unwrap(), Ok(Some(42)));
    }

    /// Producer drop wakes a parked reader and ends the stream with `None`
    /// after any pending value is delivered.
    #[tokio::test]
    async fn async_next_delivers_pending_then_none_on_producer_drop() {
        let (tx, rx) = latest_channel::<u32>();
        let rx = AsyncLatestReceiver::new(rx);
        tx.send(1);
        drop(tx);
        assert_eq!(rx.next().await, Ok(Some(1)));
        assert_eq!(rx.next().await, Ok(None));
    }

    #[tokio::test]
    async fn async_next_wakes_none_when_producer_drops_while_parked() {
        let (tx, rx) = latest_channel::<u32>();
        let rx = AsyncLatestReceiver::new(rx);
        let waiter = tokio::spawn(async move { rx.next().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(tx);
        assert_eq!(waiter.await.unwrap(), Ok(None));
    }

    /// Consumer `close()` (an explicit cancel) ends the stream immediately and
    /// discards a buffered latest value — no post-cancel frame is ever seen.
    #[tokio::test]
    async fn close_discards_pending_value_and_yields_none() {
        let (tx, rx) = latest_channel::<u32>();
        let rx = AsyncLatestReceiver::new(rx);
        tx.send(7);
        rx.close();
        assert_eq!(rx.next().await, Ok(None));
        // A late producer send after close is ignored, still None.
        tx.send(9);
        assert_eq!(rx.next().await, Ok(None));
    }

    #[tokio::test]
    async fn close_wakes_a_parked_reader_immediately() {
        let (_tx, rx) = latest_channel::<u32>();
        let rx = Arc::new(AsyncLatestReceiver::new(rx));
        let reader = rx.clone();
        let waiter = tokio::spawn(async move { reader.next().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        rx.close();
        assert_eq!(waiter.await.unwrap(), Ok(None));
    }

    /// A second overlapping `next()` is structurally rejected rather than
    /// racing the single parked waker. Driven by hand so both futures are live
    /// at once without deadlocking.
    #[test]
    fn concurrent_next_is_rejected() {
        let (tx, rx) = latest_channel::<u32>();
        let rx = AsyncLatestReceiver::new(rx);
        let mut cx = Context::from_waker(Waker::noop());

        let mut first = pin!(rx.next());
        assert!(first.as_mut().poll(&mut cx).is_pending());

        let mut second = pin!(rx.next());
        assert_eq!(
            second.as_mut().poll(&mut cx),
            Poll::Ready(Err(ConcurrentNext))
        );

        // Once the first future completes (its guard drops as the async fn
        // returns), a later call proceeds normally.
        tx.send(5);
        assert_eq!(first.as_mut().poll(&mut cx), Poll::Ready(Ok(Some(5))));
        let mut third = pin!(rx.next());
        assert!(third.as_mut().poll(&mut cx).is_pending());
    }

    /// A `next()` future dropped mid-poll (foreign task cancellation) releases
    /// the single-reader guard so a fresh `next()` can proceed.
    #[test]
    fn dropping_a_pending_next_releases_the_reader_guard() {
        let (_tx, rx) = latest_channel::<u32>();
        let rx = AsyncLatestReceiver::new(rx);
        let mut cx = Context::from_waker(Waker::noop());
        {
            let mut first = pin!(rx.next());
            assert!(first.as_mut().poll(&mut cx).is_pending());
        }
        let mut second = pin!(rx.next());
        assert!(second.as_mut().poll(&mut cx).is_pending());
    }
}
