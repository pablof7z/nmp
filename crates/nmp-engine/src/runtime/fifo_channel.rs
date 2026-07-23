//! A waker-aware FIFO fact channel (#680).
//!
//! Unlike the latest-wins single-slot mailbox (`diagnostics_channel.rs`), this
//! preserves **every** value in order. Receipt (`WriteStatus`) and
//! follow-action-status transitions are per-lane facts where a later value does
//! not subsume an earlier one (`Sent{relay}`, `AwaitingAuth`, per-attempt
//! ordinals …), so they must not conflate.
//!
//! Retained cardinality is bounded directly by [`FACT_CHANNEL_CAPACITY`].
//! Durable retry has no attempt-count ceiling, so lifecycle cardinality cannot
//! be used as a memory bound. If a consumer falls behind the finite queue, the
//! channel keeps its already-buffered prefix, rejects further sends, then
//! surfaces [`FifoNextError::Lagged`] after that prefix drains. Receipt callers
//! can reattach to the durable outbox/redb source of truth; no fact is silently
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
pub fn fifo_channel<T>() -> (FifoSender<T>, FifoReceiver<T>) {
    let inner = Arc::new(Inner {
        queue: Mutex::new(Queue {
            items: VecDeque::new(),
            state: FifoState::Open,
            waker: None,
            close_hook: None,
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
    /// has cancelled or this finite queue has lagged. On the first overflow,
    /// the already-buffered prefix is retained and the rejected value remains
    /// available through its durable owner rather than being claimed live.
    pub fn send(&self, value: T) -> bool {
        let (accepted, waker) = {
            let mut queue = self.inner.queue.lock().unwrap();
            if queue.state != FifoState::Open {
                return false;
            }
            if queue.items.len() == FACT_CHANNEL_CAPACITY {
                queue.state = FifoState::Lagged;
                self.inner.cvar.notify_all();
                (false, queue.waker.take())
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
        let _guard = ReadingGuard(&self.reading);
        poll_fn(|cx| self.rx.poll_recv(cx)).await
    }

    /// Idempotent consumer-initiated close; wakes a parked `next()` to `None`.
    pub fn close(&self) {
        self.rx.close();
    }
}

struct ReadingGuard<'a>(&'a AtomicBool);

impl Drop for ReadingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_preserves_order_and_never_conflates() {
        let (tx, rx) = fifo_channel::<u32>();
        for value in 0..5 {
            tx.send(value);
        }
        for value in 0..5 {
            assert_eq!(rx.recv(), Ok(value));
        }
        drop(tx);
        assert_eq!(rx.recv(), Err(FifoRecvError::Closed));
    }

    #[tokio::test]
    async fn async_next_drains_backlog_then_none_on_producer_drop() {
        let (tx, rx) = fifo_channel::<u32>();
        tx.send(1);
        tx.send(2);
        drop(tx);
        let rx = rx.into_async();
        assert_eq!(rx.next().await, Ok(Some(1)));
        assert_eq!(rx.next().await, Ok(Some(2)));
        assert_eq!(rx.next().await, Ok(None));
    }

    #[tokio::test]
    async fn async_next_wakes_on_send() {
        let (tx, rx) = fifo_channel::<u32>();
        let rx = rx.into_async();
        let waiter = tokio::spawn(async move { rx.next().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(9);
        assert_eq!(waiter.await.unwrap(), Ok(Some(9)));
    }

    #[tokio::test]
    async fn close_drops_backlog_and_yields_none() {
        let (tx, rx) = fifo_channel::<u32>();
        tx.send(1);
        tx.send(2);
        let rx = rx.into_async();
        rx.close();
        assert_eq!(rx.next().await, Ok(None));
    }

    #[test]
    fn close_hook_runs_exactly_once_on_close_or_drop() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_tx, rx) = fifo_channel::<u32>();
        let hook_calls = Arc::clone(&calls);
        rx.set_close_hook(move || {
            hook_calls.fetch_add(1, Ordering::SeqCst);
        });
        rx.close();
        rx.close();
        drop(rx);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let (_tx, dropped) = fifo_channel::<u32>();
        let hook_calls = Arc::clone(&calls);
        dropped.set_close_hook(move || {
            hook_calls.fetch_add(1, Ordering::SeqCst);
        });
        drop(dropped);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn concurrent_next_is_rejected() {
        use std::future::Future;
        use std::pin::pin;
        use std::task::Waker;

        let (_tx, rx) = fifo_channel::<u32>();
        let rx = rx.into_async();
        let mut cx = Context::from_waker(Waker::noop());
        let mut first = pin!(rx.next());
        assert!(first.as_mut().poll(&mut cx).is_pending());
        let mut second = pin!(rx.next());
        assert_eq!(
            second.as_mut().poll(&mut cx),
            Poll::Ready(Err(FifoNextError::ConcurrentNext))
        );
    }

    #[tokio::test]
    async fn paused_consumer_is_finitely_buffered_then_told_to_replay() {
        let (tx, rx) = fifo_channel::<usize>();
        for value in 0..FACT_CHANNEL_CAPACITY {
            assert!(tx.send(value));
        }
        assert!(
            !tx.send(FACT_CHANNEL_CAPACITY),
            "the first fact beyond the finite live bound is rejected"
        );
        assert!(
            !tx.send(FACT_CHANNEL_CAPACITY + 1),
            "a lagged channel never resumes accepting an incomplete suffix"
        );

        let rx = rx.into_async();
        for value in 0..FACT_CHANNEL_CAPACITY {
            assert_eq!(rx.next().await, Ok(Some(value)));
        }
        assert_eq!(rx.next().await, Err(FifoNextError::Lagged));
    }
}
