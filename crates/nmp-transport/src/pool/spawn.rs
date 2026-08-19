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
        builder.spawn(move || crate::thread_census::run_counted_thread(task))
    }
}

pub(super) fn system_spawner() -> Arc<dyn ThreadSpawner> {
    Arc::new(SystemThreadSpawner)
}

