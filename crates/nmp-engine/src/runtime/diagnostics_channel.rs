//! A single-slot "latest value wins" mailbox (M5 plan §1.2 step 4). The
//! diagnostics stream is a projection of engine-global state that can be
//! recomputed on every recompile/EOSE — a slow consumer should see the MOST
//! RECENT snapshot next, never a growing backlog of stale ones (unlike
//! `RowsMsg`'s plain `mpsc` channel, where every row delta matters and none
//! may be dropped). `recv` still blocks via `Condvar::wait` (D8: never a
//! poll loop) and reports sender-disconnect exactly like `mpsc::Receiver::
//! recv`'s `Err`, so `dispatch_effect`/the FFI drain thread can treat it the
//! same way.

use std::sync::{Arc, Condvar, Mutex};

struct Slot<T> {
    value: Option<T>,
    closed: bool,
}

struct Inner<T> {
    slot: Mutex<Slot<T>>,
    cvar: Condvar,
}

/// The producer half. `EngineThread`'s dispatch loop holds one per
/// registered diagnostics observer (see `runtime::Handle::
/// observe_diagnostics`); dropping it (on `Cmd::UnobserveDiagnostics`)
/// closes the mailbox.
pub struct LatestSender<T> {
    inner: Arc<Inner<T>>,
}

/// The consumer half, returned to the caller of `observe_diagnostics`
/// (wrapped again by `nmp-ffi`'s drain thread for the Swift bridge).
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
}
