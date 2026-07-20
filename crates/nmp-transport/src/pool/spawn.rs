use std::io;
use std::sync::Arc;
use std::thread::{Builder, JoinHandle};

pub(super) type ThreadTask = Box<dyn FnOnce() + Send + 'static>;

pub(super) trait ThreadSpawner: Send + Sync {
    fn spawn(&self, builder: Builder, task: ThreadTask) -> io::Result<JoinHandle<()>>;
}

#[derive(Debug, Default)]
pub(super) struct SystemThreadSpawner;

impl ThreadSpawner for SystemThreadSpawner {
    fn spawn(&self, builder: Builder, task: ThreadTask) -> io::Result<JoinHandle<()>> {
        // #680/#704: count every real transport OS thread (translator, relay
        // reaper, per-relay workers, verifier workers) into the whole-engine
        // instrumentation so the thread-scaling and teardown falsifiers have no
        // blind spot. Counting happens INSIDE the thread body (spawn + exit
        // paired on the same thread) so the live gauge cannot race and returns
        // to baseline when a pool is dropped. Injected test spawners do not
        // bump it.
        builder.spawn(move || nmp_executor::run_counted_thread(task))
    }
}

pub(super) fn system_spawner() -> Arc<dyn ThreadSpawner> {
    Arc::new(SystemThreadSpawner)
}

#[cfg(test)]
pub(super) mod test_support {
    use super::{ThreadSpawner, ThreadTask};
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread::{Builder, JoinHandle};

    struct LiveGuard(Arc<AtomicUsize>);

    impl Drop for LiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Deterministic OS-spawn refusal seam: allow exactly `successful_spawns`
    /// thread creations, then return `WouldBlock` without starting a thread.
    pub(in crate::pool) struct RefusingThreadSpawner {
        remaining: AtomicUsize,
        live: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl RefusingThreadSpawner {
        pub(in crate::pool) fn after(successful_spawns: usize) -> Self {
            Self {
                remaining: AtomicUsize::new(successful_spawns),
                live: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
            }
        }

        pub(in crate::pool) fn live(&self) -> usize {
            self.live.load(Ordering::SeqCst)
        }

        pub(in crate::pool) fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }
    }

    impl ThreadSpawner for RefusingThreadSpawner {
        fn spawn(&self, builder: Builder, task: ThreadTask) -> io::Result<JoinHandle<()>> {
            self.remaining
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::WouldBlock, "injected thread pressure")
                })?;
            let live = Arc::clone(&self.live);
            let peak = Arc::clone(&self.peak);
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            match builder.spawn(move || {
                let _live = LiveGuard(live);
                task();
            }) {
                Ok(join) => Ok(join),
                Err(error) => {
                    self.live.fetch_sub(1, Ordering::SeqCst);
                    Err(error)
                }
            }
        }
    }
}
