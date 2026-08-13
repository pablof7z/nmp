//! Process-wide OS-thread census for NMP (#704-reduced, #1452).
//!
//! Transport is the lowest crate that both the pool and the engine can share
//! without a cycle. These counters exist so tests can prove observations do
//! not create threads and shutdown leaves no orphans. They are not an
//! executor, reservation, or admission surface.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Process-wide monotonic count of real OS threads NMP has spawned through
/// the instrumented spawn paths.
static NMP_THREADS_SPAWNED: AtomicU64 = AtomicU64::new(0);

/// Process-wide live gauge. Signed so a benign spawn/exit reordering cannot
/// wrap; [`nmp_threads_live`] clamps at 0.
static NMP_THREADS_LIVE: AtomicI64 = AtomicI64::new(0);

/// Record that one real NMP-owned OS thread was just created.
pub fn note_thread_spawn() {
    NMP_THREADS_SPAWNED.fetch_add(1, Ordering::Relaxed);
    NMP_THREADS_LIVE.fetch_add(1, Ordering::Relaxed);
}

/// Record that one real NMP-owned OS thread just exited.
pub fn note_thread_exit() {
    NMP_THREADS_LIVE.fetch_sub(1, Ordering::Relaxed);
}

/// Run a thread body with both counters maintained on this thread.
pub fn run_counted_thread<F: FnOnce()>(body: F) {
    note_thread_spawn();
    let _exit = ThreadExitGuard;
    body();
}

/// Drop-guard that decrements the live gauge exactly once.
pub struct ThreadExitGuard;

impl Drop for ThreadExitGuard {
    fn drop(&mut self) {
        note_thread_exit();
    }
}

/// Monotonic count of real NMP-owned OS threads spawned so far this process.
#[must_use]
pub fn nmp_threads_spawned() -> u64 {
    NMP_THREADS_SPAWNED.load(Ordering::Relaxed)
}

/// Live NMP-owned OS threads, clamped at 0.
#[must_use]
pub fn nmp_threads_live() -> u64 {
    NMP_THREADS_LIVE.load(Ordering::Relaxed).max(0) as u64
}
