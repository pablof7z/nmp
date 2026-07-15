//! A single-slot "latest value wins" mailbox (M5 plan §1.2 step 4). It backs
//! self-contained diagnostics snapshots and bounded history snapshots: a
//! slow consumer sees the MOST RECENT state next, never a growing backlog of
//! stale frames. Ordinary `RowsMsg` remains on plain `mpsc` because its raw
//! incremental deltas cannot be dropped. `recv` still blocks via
//! `Condvar::wait` (D8: never a poll loop) and reports sender-disconnect like
//! `mpsc::Receiver::recv`'s `Err`.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

struct Slot<T> {
    value: Option<T>,
    closed: bool,
}

struct Inner<T> {
    slot: Mutex<Slot<T>>,
    cvar: Condvar,
}

/// The producer half. Dropping it closes the mailbox and wakes its receiver.
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
            closed: false,
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
    /// Overwrite the slot with `value` and wake a blocked receiver. A value
    /// the receiver had not yet consumed is silently dropped — the whole
    /// point of "latest wins" (see module doc).
    pub fn send(&self, value: T) {
        let mut slot = self.inner.slot.lock().unwrap();
        slot.value = Some(value);
        self.inner.cvar.notify_one();
    }
}

impl<T> Drop for LatestSender<T> {
    fn drop(&mut self) {
        let mut slot = self.inner.slot.lock().unwrap();
        slot.closed = true;
        self.inner.cvar.notify_one();
    }
}

impl<T> LatestReceiver<T> {
    /// Block until a value is available (D8-compliant `Condvar::wait`,
    /// never a poll loop), returning `None` once the paired `LatestSender`
    /// has been dropped and no value remains unconsumed — mirrors
    /// `mpsc::Receiver::recv`'s `Err` on sender disconnect.
    pub fn recv(&self) -> Option<T> {
        let mut slot = self.inner.slot.lock().unwrap();
        loop {
            if let Some(value) = slot.value.take() {
                return Some(value);
            }
            if slot.closed {
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
            .wait_timeout_while(slot, timeout, |slot| slot.value.is_none() && !slot.closed)
            .unwrap();
        if let Some(value) = slot.value.take() {
            return Ok(value);
        }
        if slot.closed {
            Err(RecvTimeoutError::Disconnected)
        } else if wait.timed_out() {
            Err(RecvTimeoutError::Timeout)
        } else {
            unreachable!("condvar wait ended without a value, close, or timeout")
        }
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
}
