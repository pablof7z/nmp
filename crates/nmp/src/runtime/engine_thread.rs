use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel as cb;
use nmp_store::RedbStore;
use nmp_transport::{Pool, PoolConfig, PoolEvent};
use nostr::RelayUrl;

use crate::relay_information_service::RelayInformationService;
use crate::session::RestoredSession;

#[cfg(test)]
use super::AddSignerError;
use super::{
    engine_loop, pool_bridge_loop, Cmd, EngineClock, EnginePoolRuntime, EnginePoolSink,
    EngineWiring, Handle, SIGN_EVENT_COMPLETION_OP,
};
#[cfg(test)]
use nostr::{Timestamp, UnsignedEvent};

/// One dedicated engine OS thread (§2 position 2) plus the pool and AUTH
/// release bridge threads that feed it. Returned alongside the [`Handle`]
/// the app actually uses; kept around only so a caller (chiefly tests) can
/// deterministically `join` every thread after triggering
/// [`Handle::shutdown`].
pub struct EngineThread {
    engine_join: Option<JoinHandle<()>>,
    bridge_join: Option<JoinHandle<()>>,
    drain_inbox: Sender<Cmd>,
    /// #704: the engine-owned adapter runtime. Shut down from the join thread
    /// (never a worker) after the reducer stops spawning; dropping the last
    /// `Arc` aborts remaining adapter tasks, firing their Drop guards.
    runtime: Arc<tokio::runtime::Runtime>,
    /// The one value every `Tick` this thread dispatches reads its instant
    /// from. See [`EngineClock`] for why the runtime reads a clock at all
    /// instead of calling `Timestamp::now()` at each site.
    clock: EngineClock,
    #[cfg(test)]
    runtime_threads: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
pub(super) static RUNTIME_LIFECYCLE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
struct RuntimeThreadCountGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl RuntimeThreadCountGuard {
    fn enter(counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self { counter }
    }
}

#[cfg(test)]
impl Drop for RuntimeThreadCountGuard {
    fn drop(&mut self) {
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Supported construction failure for the engine-owned thread graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineThreadError {
    ThreadUnavailable {
        component: String,
        reason: String,
    },
    /// The serialized observation-open transaction could not establish its
    /// initial canonical projection. No observation owner or receiver escaped.
    ObservationUnavailable {
        reason: String,
    },
    RelayBudgetOverflow {
        relay_limit: usize,
    },
    EngineShuttingDown,
}

impl std::fmt::Display for EngineThreadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ThreadUnavailable { component, reason } => {
                write!(f, "{component} thread unavailable: {reason}")
            }
            Self::ObservationUnavailable { reason } => {
                write!(f, "observation unavailable: {reason}")
            }
            Self::RelayBudgetOverflow { relay_limit } => write!(
                f,
                "relay worker budget {relay_limit} cannot represent its retirement envelope"
            ),
            Self::EngineShuttingDown => f.write_str("engine is shutting down"),
        }
    }
}

impl std::error::Error for EngineThreadError {}

fn pool_build_error(error: nmp_transport::PoolBuildError) -> EngineThreadError {
    match error {
        nmp_transport::PoolBuildError::ThreadUnavailable(error) => {
            EngineThreadError::ThreadUnavailable {
                component: error.role.to_string(),
                reason: error.reason,
            }
        }
        nmp_transport::PoolBuildError::RelayBudgetOverflow { max_relays } => {
            EngineThreadError::RelayBudgetOverflow {
                relay_limit: max_relays,
            }
        }
    }
}

pub const DEFAULT_MAX_AUTH_CAPABILITIES: usize = 64;

/// #704: fixed worker-thread count of the ONE engine-owned adapter runtime.
/// Two workers (not one — a single worker makes any accidental blocking call a
/// total outage; not more — the adapter work is µs-scale and every task yields
/// at each `.await`). Every adapter operation is an async task that holds no
/// OS thread while waiting; there is NO admission capacity, census, or
/// per-operation `ThreadUnavailable` anywhere in the SDK.
const ADAPTER_RUNTIME_WORKERS: usize = 2;

/// Finite admission limit for live AUTH policy/signer registrations. Unlike
/// legacy zero-valued relay settings, zero AUTH capabilities intentionally
/// admits none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub max_auth_capabilities: usize,
    /// The publish attempt ceiling (#1031) threaded from
    /// [`EngineConfig::max_publish_attempts`](crate::EngineConfig).
    pub max_publish_attempts: u64,
    #[cfg(feature = "nip65")]
    pub(crate) nip65_sources: Vec<RelayUrl>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_auth_capabilities: DEFAULT_MAX_AUTH_CAPABILITIES,
            max_publish_attempts: crate::config::DEFAULT_MAX_PUBLISH_ATTEMPTS,
            #[cfg(feature = "nip65")]
            nip65_sources: Vec::new(),
        }
    }
}

impl EngineThread {
    /// Spawn the engine thread, pool bridge, and fixed adapter runtime.
    /// The `store` is constructed by the caller but moved whole into the engine
    /// thread's closure and built into `EngineCore` there — they never cross
    /// back out, which is what lets `EngineCore` itself stay `!Send`-friendly
    /// (only `Send + 'static` values ever cross the thread boundary, exactly
    /// once, at spawn time). The engine starts with an EMPTY `SignerRegistry`
    /// (zero accounts, read-only) — matching a logged-out launch (M4 §5);
    /// the caller registers accounts afterward via [`Handle::add_signer`] and
    /// picks one via [`Handle::set_current_account`].
    pub fn spawn(
        store: RedbStore,
        cap: usize,
        pool_config: PoolConfig,
    ) -> Result<(Self, Handle), EngineThreadError> {
        Self::spawn_with_runtime_config(store, cap, pool_config, RuntimeConfig::default())
    }

    /// Spawn a headless runtime over a static fact snapshot.
    ///
    /// This exists for deterministic falsifiers. Production assembly owns
    /// the private mutable fact store and uses [`Self::spawn`].
    #[doc(hidden)]
    pub fn spawn_with_fixture_routing_facts(
        store: RedbStore,
        facts: nmp_router::FixtureRoutingFacts,
        cap: usize,
        pool_config: PoolConfig,
    ) -> Result<(Self, Handle), EngineThreadError> {
        Self::spawn_with_routing_facts_and_runtime_config(
            store,
            crate::core::RoutingFactStore::from_fixture(facts),
            cap,
            pool_config,
            RuntimeConfig::default(),
            RestoredSession::empty(),
        )
    }

    pub fn spawn_with_runtime_config(
        store: RedbStore,
        cap: usize,
        pool_config: PoolConfig,
        runtime_config: RuntimeConfig,
    ) -> Result<(Self, Handle), EngineThreadError> {
        Self::spawn_with_routing_facts_and_runtime_config(
            store,
            crate::core::RoutingFactStore::default(),
            cap,
            pool_config,
            runtime_config,
            RestoredSession::empty(),
        )
    }

    pub(crate) fn spawn_with_routing_facts_and_runtime_config(
        store: RedbStore,
        routing_facts: crate::core::RoutingFactStore,
        cap: usize,
        mut pool_config: PoolConfig,
        runtime_config: RuntimeConfig,
        initial_session: RestoredSession,
    ) -> Result<(Self, Handle), EngineThreadError> {
        // #704: the ONE engine-owned adapter runtime. A fixed 2-worker
        // multi-thread tokio runtime hosts every adapter task; each worker
        // thread start bumps the process-wide OS-thread counter. Build failure
        // is an engine-start infrastructure error.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(ADAPTER_RUNTIME_WORKERS)
            .enable_all()
            .thread_name("nmp-adapter")
            .on_thread_start(nmp_transport::thread_census::note_thread_spawn)
            .on_thread_stop(nmp_transport::thread_census::note_thread_exit)
            .build()
            .map(Arc::new)
            .map_err(|error| EngineThreadError::ThreadUnavailable {
                component: "adapter runtime".to_string(),
                reason: error.to_string(),
            })?;
        // One limit owns both compilation and connection admission. Legacy
        // zero values select the finite default; conflicting mechanism-test
        // inputs fail closed to the smaller non-zero ceiling.
        let cap = match (cap, pool_config.max_relays) {
            (0, 0) => nmp_transport::DEFAULT_MAX_RELAYS,
            (0, pool) => pool,
            (router, 0) => router,
            (router, pool) => router.min(pool),
        };
        pool_config.max_relays = cap;
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let relay_information = RelayInformationService::new(runtime.handle().clone());
        #[cfg(test)]
        let runtime_threads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_engine_batch = pool_config.max_engine_batch.max(1);
        let max_engine_batch_bytes = pool_config.max_engine_batch_bytes.max(1);
        let max_engine_batch_wait = pool_config
            .max_engine_batch_wait
            .min(Duration::from_millis(100));
        let (pool_evt_tx, pool_evt_rx) =
            cb::bounded::<PoolEvent>(pool_config.event_sink_queue_capacity.max(1));
        let (pool_stop_tx, pool_stop_rx) = cb::bounded::<()>(0);
        // The pool's OWN mio worker threads + translator thread are interior
        // to `Pool` (harvested, HARVEST-justified in nmp-transport's own
        // docs) — this crate never touches mio/tungstenite directly.
        let pool = match Pool::new(
            pool_config,
            EnginePoolSink {
                events: pool_evt_tx,
                stopping: pool_stop_rx.clone(),
            },
        ) {
            Ok(pool) => pool,
            Err(error) => {
                return Err(pool_build_error(error));
            }
        };

        let bridge_inbox = cmd_tx.clone();
        #[cfg(test)]
        let bridge_runtime_threads = Arc::clone(&runtime_threads);
        let bridge_join = match thread::Builder::new()
            .name("nmp-engine-pool-bridge".to_string())
            .spawn(move || {
                nmp_transport::thread_census::run_counted_thread(move || {
                    #[cfg(test)]
                    let _thread_count = RuntimeThreadCountGuard::enter(bridge_runtime_threads);
                    pool_bridge_loop(
                        &pool_evt_rx,
                        &pool_stop_rx,
                        &bridge_inbox,
                        max_engine_batch,
                        max_engine_batch_bytes,
                        max_engine_batch_wait,
                    )
                })
            }) {
            Ok(join) => join,
            Err(error) => {
                pool.shutdown();
                return Err(EngineThreadError::ThreadUnavailable {
                    component: "engine pool bridge".to_string(),
                    reason: error.to_string(),
                });
            }
        };

        let clock = EngineClock::wired(cmd_tx.clone());
        let (startup_ready_tx, startup_ready_rx) = mpsc::channel();
        let engine_clock = clock.clone();
        let self_inbox = cmd_tx.clone();
        let engine_pool = pool.clone();
        let engine_stop = pool_stop_tx.clone();
        let engine_runtime = Arc::clone(&runtime);
        let engine_relay_information = relay_information.clone();
        #[cfg(test)]
        let engine_runtime_threads = Arc::clone(&runtime_threads);
        let engine_join =
            match thread::Builder::new()
                .name("nmp-engine".to_string())
                .spawn(move || {
                    nmp_transport::thread_census::run_counted_thread(move || {
                        #[cfg(test)]
                        let _thread_count = RuntimeThreadCountGuard::enter(engine_runtime_threads);
                        engine_loop(
                            store,
                            routing_facts,
                            cap,
                            initial_session,
                            EnginePoolRuntime {
                                pool: engine_pool,
                                stop: engine_stop,
                                runtime: engine_runtime,
                                relay_information: engine_relay_information,
                                max_auth_capabilities: runtime_config.max_auth_capabilities,
                                max_publish_attempts: runtime_config.max_publish_attempts,
                                #[cfg(feature = "nip65")]
                                nip65_sources: runtime_config.nip65_sources,
                            },
                            EngineWiring {
                                clock: &engine_clock,
                                cmd_rx: &cmd_rx,
                                self_inbox: &self_inbox,
                                startup_ready: startup_ready_tx,
                            },
                        )
                    })
                }) {
                Ok(join) => join,
                Err(error) => {
                    drop(pool_stop_tx);
                    pool.shutdown();
                    let _ = bridge_join.join();
                    return Err(EngineThreadError::ThreadUnavailable {
                        component: "engine runtime".to_string(),
                        reason: error.to_string(),
                    });
                }
            };
        if startup_ready_rx.recv().is_err() {
            drop(pool_stop_tx);
            pool.shutdown();
            let _ = engine_join.join();
            let _ = bridge_join.join();
            return Err(EngineThreadError::ThreadUnavailable {
                component: "engine runtime".to_string(),
                reason: "engine exited before startup recovery completed".to_string(),
            });
        }
        drop(pool);

        Ok((
            Self {
                engine_join: Some(engine_join),
                bridge_join: Some(bridge_join),
                drain_inbox: cmd_tx.clone(),
                runtime,
                clock,
                #[cfg(test)]
                runtime_threads,
            },
            Handle {
                inbox: cmd_tx,
                relay_information,
            },
        ))
    }

    /// #704: the engine-owned adapter runtime handle. Protocol adapters
    /// (follow-action and optional signer-provider handshakes) spawn their
    /// async tasks here instead of reserving a slot on the deleted
    /// blocking-adapter executor. Exposed on [`EngineThread`] (not the narrow
    /// app-facing [`Handle`]) so it stays hidden mechanism, never an app
    /// scheduling verb.
    #[must_use]
    pub fn adapter_runtime(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    /// This thread's wall clock, so an owner that has to STATE what time the
    /// engine is running at can.
    ///
    /// Exposed on [`EngineThread`] rather than on the app-facing [`Handle`]
    /// for the same reason [`Self::adapter_runtime`] is: an app has no
    /// business deciding what time it is, and a value only the thread's owner
    /// can reach cannot become an app contract by accident. Unpinned by
    /// default, so a caller that never touches it gets `Timestamp::now()`
    /// everywhere, byte for byte what the runtime did before this existed.
    #[must_use]
    pub fn clock(&self) -> EngineClock {
        self.clock.clone()
    }

    /// Block until the engine and pool-bridge threads have exited. Only
    /// returns once a [`Handle::shutdown`] has actually been observed by the
    /// engine thread (which then tears down its `Pool` clone, allowing the
    /// pool bridge to disconnect) — callers that never shut down any `Handle`
    /// block here forever, matching `Pool::shutdown`'s own join discipline.
    ///
    /// #704: when called from a per-operation sign-event completion thread that
    /// is calling `join()` reentrantly, the reducer exempts only that exact
    /// operation from the shutdown drain (read from the completion-thread-local
    /// `SIGN_EVENT_COMPLETION_OP`). The adapter runtime is then shut down from
    /// THIS join thread (never a worker) by dropping the last `Arc` after the
    /// reducer thread has exited — remaining adapter task futures are dropped,
    /// firing their Drop guards (delivering `Cancelled`/`Disconnected` to any
    /// foreign completion exactly once).
    pub fn join(mut self) {
        if let Some(op_id) = SIGN_EVENT_COMPLETION_OP.with(|op| op.get()) {
            let _ = self.drain_inbox.send(Cmd::ExemptSignEventDrain(op_id));
        }
        if let Some(h) = self.engine_join.take() {
            let _ = h.join();
        }
        if let Some(h) = self.bridge_join.take() {
            let _ = h.join();
        }
        // The reducer thread has exited (its runtime `Arc` clone dropped), so
        // this is the last `Arc`. Shut the adapter runtime down on a FRESH
        // dedicated OS thread and join it: dropping a `tokio::runtime::Runtime`
        // panics if done inside another runtime's context (e.g. an app or a
        // `#[tokio::test]` that owns the calling thread), and `join()` may be
        // called from exactly there. On the fresh thread the drop is legal and
        // fires every parked adapter task's Drop guard, delivering
        // `Cancelled`/`Disconnected` to each foreign completion exactly once.
        let runtime = self.runtime;
        let _ = thread::Builder::new()
            .name("nmp-adapter-shutdown".to_string())
            .spawn(move || drop(runtime))
            .map(|handle| handle.join());
    }
}

#[cfg(test)]
mod reentrant_shutdown_tests;
