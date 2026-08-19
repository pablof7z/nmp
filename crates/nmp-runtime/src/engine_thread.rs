use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel as cb;
use nmp_store::RedbStore;
use nmp_transport::{Pool, PoolConfig, PoolEvent};
use nostr::RelayUrl;

use nmp_engine::core::AuthorRouteProvider;
use nmp_nip11::RelayInformationService;

use crate::session::RestoredSession;

use super::{
    engine_loop, pool_bridge_loop, sign_event, Cmd, EngineClock, EnginePoolRuntime, EnginePoolSink,
    EngineWiring, Handle,
};

/// Engine-side adapter that closes the verify gate's durable-dedup seam over
/// the store (#1677). It wraps a [`nmp_store::StoreSigReader`] — a shared
/// `Arc<Database>` cut from the store at engine construction — so the trust
/// gate can byte-compare a relay-replayed id against the stored known-good
/// signature without borrowing the engine's `RedbStore` and without a
/// schnorr check. A store read error is non-fatal here: it returns `None`
/// and the candidate falls through to schnorr.
struct StoreKnownSig(nmp_store::StoreSigReader);

impl nmp_transport::KnownSig for StoreKnownSig {
    fn known_signature(&self, id: &nostr::EventId) -> Option<nostr::secp256k1::schnorr::Signature> {
        self.0.known_signature(id)
    }
}

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
    /// How many times this engine thread's loop has come around to arm its
    /// wait. See [`EngineThread::wait_arms`]. The engine loop publishes the
    /// count unconditionally — a falsifier that only holds for a specially
    /// built loop proves nothing about the shipped one — and only this
    /// reader's end of the `Arc` is gated.
    #[cfg(feature = "test-instrumentation")]
    wait_arms: Arc<std::sync::atomic::AtomicU64>,
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
    MissingReplaceableCapability {
        program: [u8; 16],
        format: [u8; 16],
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
            Self::MissingReplaceableCapability { program, format } => write!(
                f,
                "store retains replaceable operations for missing compiled capability program {:02x?} format {:02x?}",
                program,
                format
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
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub max_auth_capabilities: usize,
    /// The publish attempt ceiling (#1031) threaded from
    /// [`EngineConfig::max_publish_attempts`](crate::EngineConfig).
    pub max_publish_attempts: u64,
    /// Operator-configured relays the router reads as neutral routing facts,
    /// already parsed by the facade.
    ///
    /// Plain values rather than a prebuilt store: `RoutingFactStore` is the
    /// engine's own type, and the facade used to have to name it purely to
    /// pass these two lists through it (#1142 boundary cleanup).
    pub app_relays: Vec<RelayUrl>,
    pub fallback_relays: Vec<RelayUrl>,
    /// The wall clock this engine's reducer reads, supplied by the host the
    /// same way its `AuthorRouteProvider` is. Default is unpinned: every read
    /// is `Timestamp::now()` and the caller wrote no clock code at all.
    ///
    /// Installed BEFORE the engine thread starts, which is the whole reason
    /// it is construction input rather than a value handed back afterwards:
    /// store recovery reads it, and by the time a caller could reach a
    /// value returned from construction, recovery has already run.
    pub clock: EngineClock,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_auth_capabilities: DEFAULT_MAX_AUTH_CAPABILITIES,
            max_publish_attempts: nmp_engine::publish_queue::DEFAULT_MAX_PUBLISH_ATTEMPTS,
            app_relays: Vec::new(),
            fallback_relays: Vec::new(),
            clock: EngineClock::new(),
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
        Self::spawn_with_runtime_config(
            store,
            cap,
            pool_config,
            RuntimeConfig::default(),
            Vec::new(),
            None,
        )
    }

    pub fn spawn_with_runtime_config(
        store: RedbStore,
        cap: usize,
        pool_config: PoolConfig,
        runtime_config: RuntimeConfig,
        capabilities: Vec<nmp_grammar::ReplaceableMaterializerSpec>,
        route_provider: Option<Box<dyn AuthorRouteProvider>>,
    ) -> Result<(Self, Handle), EngineThreadError> {
        Self::spawn_with_runtime_config_and_session(
            store,
            cap,
            pool_config,
            runtime_config,
            RestoredSession::empty(),
            capabilities,
            route_provider,
        )
    }

    /// The ordinary door. Routing facts are built here, inside the engine,
    /// from the operator relays the facade parsed — the facade never names
    /// `RoutingFactStore`.
    ///
    /// `route_provider` is the application's chosen author-route algorithm,
    /// beside its chosen capabilities and fixed for this engine's life. It is
    /// an `Option`, never a `Vec`: an author's routes are replaced whole, so
    /// two providers would silently last-write-win with no merge rule anyone
    /// could state. `None` discovers no routes at all — operator lanes and
    /// explicit routes still carry everything they carry.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_runtime_config_and_session(
        store: RedbStore,
        cap: usize,
        pool_config: PoolConfig,
        runtime_config: RuntimeConfig,
        initial_session: RestoredSession,
        capabilities: Vec<nmp_grammar::ReplaceableMaterializerSpec>,
        route_provider: Option<Box<dyn AuthorRouteProvider>>,
    ) -> Result<(Self, Handle), EngineThreadError> {
        let routing_facts = nmp_engine::core::RoutingFactStore::new(
            runtime_config.app_relays.clone(),
            runtime_config.fallback_relays.clone(),
        );
        Self::spawn_with_facts(
            store,
            routing_facts,
            cap,
            pool_config,
            runtime_config,
            initial_session,
            capabilities,
            route_provider,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_with_facts(
        store: RedbStore,
        routing_facts: nmp_engine::core::RoutingFactStore,
        cap: usize,
        mut pool_config: PoolConfig,
        runtime_config: RuntimeConfig,
        initial_session: RestoredSession,
        capabilities: Vec<nmp_grammar::ReplaceableMaterializerSpec>,
        route_provider: Option<Box<dyn AuthorRouteProvider>>,
    ) -> Result<(Self, Handle), EngineThreadError> {
        let supplied: std::collections::HashSet<_> = capabilities
            .iter()
            .map(|spec| (spec.program(), spec.format()))
            .collect();
        match store.required_replaceable_programs() {
            Ok(required) => {
                if let Some((program, format)) = required
                    .into_iter()
                    .find(|(program, format)| !supplied.contains(&(program.0, format.0)))
                {
                    return Err(EngineThreadError::MissingReplaceableCapability {
                        program: program.0,
                        format: format.0,
                    });
                }
            }
            Err(error) => {
                return Err(EngineThreadError::ThreadUnavailable {
                    component: "replaceable capability census".to_string(),
                    reason: error.to_string(),
                });
            }
        }
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
        let wait_arms = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let max_engine_batch = pool_config.max_engine_batch.max(1);
        let max_engine_batch_bytes = pool_config.max_engine_batch_bytes.max(1);
        let max_engine_batch_wait = pool_config
            .max_engine_batch_wait
            .min(Duration::from_millis(100));
        let (pool_evt_tx, pool_evt_rx) =
            cb::bounded::<PoolEvent>(pool_config.event_sink_queue_capacity.max(1));
        let (pool_stop_tx, pool_stop_rx) = cb::bounded::<()>(0);
        // #1677: the engine owns the trust gate. It cuts a durable sig reader
        // from the store (a shared `Arc<Database>`, MVCC — no writer block)
        // and builds the verifier before `Pool::new`. The store is still
        // moved into the engine thread below; the reader is independent.
        let known_sig: Arc<dyn nmp_transport::KnownSig> =
            Arc::new(StoreKnownSig(store.share_sig_reader().map_err(
                |error| EngineThreadError::ThreadUnavailable {
                    component: "verifier".to_string(),
                    reason: error.to_string(),
                },
            )?));
        let verifier =
            nmp_transport::Verifier::new(nmp_transport::VerifyConfig::default(), known_sig)
                .map_err(|error| EngineThreadError::ThreadUnavailable {
                    component: "verifier".to_string(),
                    reason: error.to_string(),
                })?;
        // The pool's OWN mio worker threads + translator thread are interior
        // to `Pool` (harvested, HARVEST-justified in nmp-transport's own
        // docs) — this crate never touches mio/tungstenite directly.
        let pool = match Pool::new(
            pool_config,
            verifier,
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
        let bridge_join = match thread::Builder::new()
            .name("nmp-engine-pool-bridge".to_string())
            .spawn(move || {
                nmp_transport::thread_census::run_counted_thread(move || {
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

        // The host's clock, installed on this thread's inbox BEFORE the
        // thread starts, so a time stated before construction is the time
        // `recover_on_boot` runs at.
        let engine_clock = runtime_config.clock.clone();
        engine_clock.install(cmd_tx.clone());
        let (startup_ready_tx, startup_ready_rx) = mpsc::channel();
        let self_inbox = cmd_tx.clone();
        let engine_pool = pool.clone();
        let engine_stop = pool_stop_tx.clone();
        let engine_runtime = Arc::clone(&runtime);
        let engine_relay_information = relay_information.clone();
        let engine_wait_arms = Arc::clone(&wait_arms);
        let engine_join =
            match thread::Builder::new()
                .name("nmp-engine".to_string())
                .spawn(move || {
                    nmp_transport::thread_census::run_counted_thread(move || {
                        engine_loop(
                            store,
                            routing_facts,
                            cap,
                            initial_session,
                            capabilities,
                            EnginePoolRuntime {
                                pool: engine_pool,
                                stop: engine_stop,
                                runtime: engine_runtime,
                                relay_information: engine_relay_information,
                                max_auth_capabilities: runtime_config.max_auth_capabilities,
                                max_publish_attempts: runtime_config.max_publish_attempts,
                                route_provider,
                            },
                            EngineWiring {
                                clock: &engine_clock,
                                cmd_rx: &cmd_rx,
                                self_inbox: &self_inbox,
                                startup_ready: startup_ready_tx,
                                wait_arms: engine_wait_arms,
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
                #[cfg(feature = "test-instrumentation")]
                wait_arms,
            },
            Handle {
                inbox: cmd_tx,
                relay_information,
            },
        ))
    }

    /// How many times THIS engine thread's loop has come around to arm its
    /// wait, counted since the thread started.
    ///
    /// #1796: the direct reading of "the engine thread is parked on `recv()`",
    /// which is what the deadline-armed driver (§3.3, #39) claims when nothing
    /// is due. A parked thread has not reached the top of the loop again, so
    /// this count does not move at all while it waits; a busy-spinning
    /// `recv_timeout(0)` loop moves it once per spin, millions of times a
    /// second. The count belongs to one engine thread, so no concurrent test,
    /// engine, or ambient machine load can change what it reads — which is
    /// exactly what the process-wide `getrusage` sample it replaced could not
    /// promise.
    ///
    /// Reading it is a plain relaxed atomic load: it sends no command and
    /// wakes nothing, so sampling cannot itself disturb the parked wait it is
    /// measuring.
    #[cfg(feature = "test-instrumentation")]
    #[doc(hidden)]
    #[must_use]
    pub fn wait_arms(&self) -> u64 {
        self.wait_arms.load(std::sync::atomic::Ordering::Relaxed)
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

    /// Block until the engine and pool-bridge threads have exited. Only
    /// returns once a [`Handle::shutdown`] has actually been observed by the
    /// engine thread (which then tears down its `Pool` clone, allowing the
    /// pool bridge to disconnect) — callers that never shut down any `Handle`
    /// block here forever, matching `Pool::shutdown`'s own join discipline.
    ///
    /// #704: when called from a per-operation sign-event completion thread that
    /// is calling `join()` reentrantly, the reducer exempts only that exact
    /// operation from the shutdown drain (read from the completion-thread-local
    /// sign-event owner). The adapter runtime is then shut down from
    /// THIS join thread (never a worker) by dropping the last `Arc` after the
    /// reducer thread has exited — remaining adapter task futures are dropped,
    /// firing their Drop guards (delivering `Cancelled`/`Disconnected` to any
    /// foreign completion exactly once).
    pub fn join(mut self) {
        if let Some(op_id) = sign_event::completion_operation() {
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

