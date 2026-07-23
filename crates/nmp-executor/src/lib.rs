//! Process-wide OS-thread instrumentation (#704-reduced).
//!
//! #704 eliminated the internal task/thread admission executor: every former
//! executor user is now an async task on the engine-owned tokio runtime (see
//! `docs/design/internal-executor-elimination.md`). All that remains of this
//! crate is the process-wide thread instrumentation first added in #693 — it
//! lives here (a leaf crate every transport/engine layer can depend on) so
//! `nmp::nmp_threads_spawned`/`nmp::nmp_threads_live` keep working across the
//! whole workspace. The `Executor`/`Reservation`/`Saturated`/`ReleaseId`/census
//! surface is GONE.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Process-wide MONOTONIC count of real OS threads NMP has spawned through the
/// instrumented spawn paths (#680/#693 falsifier instrumentation). The engine
/// runtime's worker threads, the reducer/pool-bridge threads, transport
/// workers, and per-operation sign-event completion threads all count through
/// [`note_thread_spawn`] so [`nmp_threads_spawned`] counts them. It exists so a
/// test can prove the async observation architecture creates NO thread per
/// observation: opening N observation handles must leave this delta at 0.
static NMP_THREADS_SPAWNED: AtomicU64 = AtomicU64::new(0);

/// Process-wide LIVE gauge: real NMP-owned OS threads currently running.
/// [`note_thread_spawn`] increments it, [`note_thread_exit`] decrements it, so
/// after a session/engine teardown a census can prove the live count returns to
/// its baseline — i.e. no orphaned worker survives cancellation/drop/shutdown
/// (#704 review falsifier). Signed so a benign spawn/exit reordering across the
/// two counters can never wrap; [`nmp_threads_live`] clamps at 0.
static NMP_THREADS_LIVE: AtomicI64 = AtomicI64::new(0);

/// Record that one real NMP-owned OS thread was just created. Prefer
/// [`run_counted_thread`] (or a runtime's `on_thread_start`) so the paired
/// [`note_thread_exit`] cannot be forgotten.
pub fn note_thread_spawn() {
    NMP_THREADS_SPAWNED.fetch_add(1, Ordering::Relaxed);
    NMP_THREADS_LIVE.fetch_add(1, Ordering::Relaxed);
}

/// Record that one real NMP-owned OS thread just exited (pairs with
/// [`note_thread_spawn`]). Wire this to a runtime's `on_thread_stop`, or let
/// [`run_counted_thread`]/[`ThreadExitGuard`] call it on scope exit.
pub fn note_thread_exit() {
    NMP_THREADS_LIVE.fetch_sub(1, Ordering::Relaxed);
}

/// Run a thread body with both counters maintained ON THIS THREAD: the
/// monotonic spawn counter is bumped once at entry and the live gauge is held
/// up for the body's whole lifetime, decremented on return OR unwind. Pairing
/// spawn and exit on the same thread avoids any parent/child ordering race.
/// Use it INSIDE the closure handed to `thread::spawn`.
pub fn run_counted_thread<F: FnOnce()>(body: F) {
    note_thread_spawn();
    let _exit = ThreadExitGuard;
    body();
}

/// Drop-guard that decrements the live gauge exactly once (see
/// [`run_counted_thread`]). Fires on normal return and on unwind.
pub struct ThreadExitGuard;

impl Drop for ThreadExitGuard {
    fn drop(&mut self) {
        note_thread_exit();
    }
}

/// The monotonic count of real NMP-owned OS threads spawned so far this
/// process (see [`note_thread_spawn`]). The #680 thread-scaling falsifier
/// asserts the delta across opening many observations is 0.
#[must_use]
pub fn nmp_threads_spawned() -> u64 {
    NMP_THREADS_SPAWNED.load(Ordering::Relaxed)
}

/// The number of real NMP-owned OS threads currently alive (clamped at 0).
/// A teardown falsifier asserts this returns to its baseline after sessions
/// are dropped and the engine is shut down — proving no orphaned worker.
#[must_use]
pub fn nmp_threads_live() -> u64 {
    NMP_THREADS_LIVE.load(Ordering::Relaxed).max(0) as u64
}
