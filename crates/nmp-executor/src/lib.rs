//! Process-wide OS-thread-spawn instrumentation (#704-reduced).
//!
//! #704 eliminated the internal task/thread admission executor: every former
//! executor user is now an async task on the engine-owned tokio runtime (see
//! `docs/design/internal-executor-elimination.md`). All that remains of this
//! crate is the monotonic thread-spawn counter first added in #693 — it lives
//! here (a leaf crate every transport/engine layer can depend on) so
//! `nmp::nmp_threads_spawned` keeps working across the whole workspace. The
//! `Executor`/`Reservation`/`Saturated`/`ReleaseId`/census surface is GONE.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide monotonic count of real OS threads NMP has spawned through the
/// instrumented spawn paths (#680/#693 falsifier instrumentation). The engine
/// runtime's worker threads, the reducer/pool-bridge threads, transport
/// workers, and per-operation sign-event completion threads all call
/// [`note_thread_spawn`] so [`nmp_threads_spawned`] counts them. It exists so
/// a test can prove the async observation architecture creates NO thread per
/// observation: opening N observation handles must leave this delta at 0.
static NMP_THREADS_SPAWNED: AtomicU64 = AtomicU64::new(0);

/// Record that one real NMP-owned OS thread was just created.
pub fn note_thread_spawn() {
    NMP_THREADS_SPAWNED.fetch_add(1, Ordering::Relaxed);
}

/// The monotonic count of real NMP-owned OS threads spawned so far this
/// process (see [`note_thread_spawn`]). The #680 thread-scaling falsifier
/// asserts the delta across opening many observations is 0.
#[must_use]
pub fn nmp_threads_spawned() -> u64 {
    NMP_THREADS_SPAWNED.load(Ordering::Relaxed)
}
