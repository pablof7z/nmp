//! `ScriptedRelay` — the in-process, real-websocket relay every scenario's
//! world topology is built from (approach doc §2.2/§2.3). Wraps
//! `nostr_relay_builder::local::LocalRelay` (the same fixture the engine's
//! own `nmp-engine/tests/runtime_integration.rs` drives) with two things a
//! plain `LocalRelay` doesn't give a caller:
//!
//! - **behavior knobs** (`reject_writes`, `reject_queries` — a `Given` about
//!   the world, per the approach doc §2.3), wired through `LocalRelay`'s own
//!   `WritePolicy`/`QueryPolicy` plugin points;
//! - **a world-side "was this relay ever contacted" observable**
//!   (`ScriptedRelay::contacted`), bumped by those SAME policy hooks on
//!   every inbound EVENT/REQ. This is deliberately independent of the
//!   engine's own `DiagnosticsSnapshot`: a `must-never` scenario asserting
//!   "no relay outside the plan was ever contacted" must not take the
//!   engine's self-report as its only witness, or a diagnostics bug could
//!   silently make the ledger scenario un-falsifiable.
//!
//! Deliberately NOT a glob import of `nostr_relay_builder::prelude::*` in
//! the signature-facing parts of this module: that re-exports a DIFFERENT
//! `nostr` (0.45-alpha) than this workspace's pinned `nostr = "0.44.4"` --
//! see `nmp-engine/tests/runtime_integration.rs`'s identical comment. Every
//! cross-version value (keypairs, seeded events) is bridged explicitly by
//! hex/id string round-trip (`mirror_keys`).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use nostr::RelayUrl;

use nostr_relay_builder::builder::{
    LocalRelayBuilder, QueryPolicy, QueryPolicyResult, WritePolicy, WritePolicyResult,
};
use nostr_relay_builder::local::LocalRelay;
use nostr_relay_builder::prelude::{
    Event as RelayEvent, EventBuilder as RelayEventBuilder, FinalizeEvent, Keys as RelayKeys,
    MachineReadablePrefix,
};

/// World-side config staged by `Given` steps BEFORE the relay is actually
/// bound/started (relays are started lazily -- see `NmpWorld::ensure_started`
/// -- so a scenario can compose several `Given`s about the same relay in any
/// order before anything hits a real socket).
#[derive(Debug, Clone, Default)]
pub struct RelayConfig {
    pub reject_writes: bool,
    /// Approximates "never confirms end of stored events": the relay
    /// refuses the query outright (`CLOSED`, never `EOSE`), which yields the
    /// same app-observable consequence the ledger scenario cares about --
    /// this relay's coverage for any query touching it never resolves out
    /// of `Unknown`. See the module doc's contacted-log note for why this
    /// is a deliberate, documented approximation rather than a true
    /// accept-but-never-EOSE relay (that behavior is not a plugin point
    /// `nostr-relay-builder` 0.45.0-alpha.3 exposes).
    pub reject_queries: bool,
}

/// One running in-process relay + its contacted-log.
pub struct ScriptedRelay {
    pub url: RelayUrl,
    port: u16,
    relay: LocalRelay,
    contacted: Arc<AtomicU64>,
}

impl ScriptedRelay {
    /// Bind a fresh ephemeral port and start a `LocalRelay` configured per
    /// `config`. Async because `LocalRelay::run` is (it needs the ambient
    /// tokio runtime `tests/bdd.rs`'s `#[tokio::main]` provides).
    pub async fn start(config: &RelayConfig) -> Self {
        Self::start_on_port(free_port(), config).await
    }

    /// Start a `LocalRelay` on a SPECIFIC port -- the reconnect/drop-and-
    /// come-back scenarios' "relay X comes back" step rebinds the exact
    /// port a just-shut-down relay used (`self.port`), the same trick
    /// `nmp-engine/tests/runtime_integration.rs` uses, so the engine's own
    /// `Pool` reconnects to the SAME `RelayUrl` it already had open.
    pub async fn start_on_port(port: u16, config: &RelayConfig) -> Self {
        let contacted = Arc::new(AtomicU64::new(0));

        let relay = LocalRelayBuilder::default()
            .addr(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .port(port)
            .write_policy(LoggingWritePolicy {
                contacted: contacted.clone(),
                reject: config.reject_writes,
            })
            .query_policy(LoggingQueryPolicy {
                contacted: contacted.clone(),
                reject: config.reject_queries,
            })
            .build();
        relay
            .run()
            .await
            .expect("nmp-bdd: scripted relay must start");
        let url = RelayUrl::parse(&relay.url().await.to_string())
            .expect("nmp-bdd: scripted relay must report a parseable url");

        Self {
            url,
            port,
            relay,
            contacted,
        }
    }

    /// The port this relay is (or was, if since shut down) bound to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Seed a kind:1 text note directly into this relay's own database --
    /// pre-existing protocol state a `Given` stages, never a "when" (a live
    /// note goes through the engine's own `Handle::publish`, not this
    /// method).
    pub async fn seed_note(&self, author: &nostr::Keys, content: &str, created_at: u64) {
        let relay_keys = mirror_keys(author);
        let event: RelayEvent = RelayEventBuilder::text_note(content)
            .custom_created_at(nostr_relay_builder::prelude::Timestamp::from(created_at))
            .finalize(&relay_keys)
            .expect("nmp-bdd: fixture note must sign cleanly");
        self.relay
            .add_event(event)
            .await
            .expect("nmp-bdd: seeding a fixture note must succeed");
    }

    /// Seed a kind:3 (contact list) event -- `Given <person> follows
    /// <people>`'s pre-existing-state half (the reactive re-route
    /// scenarios' `When` publishes a NEW one live, through the engine).
    pub async fn seed_contact_list(
        &self,
        author: &nostr::Keys,
        follows: &[nostr::PublicKey],
        created_at: u64,
    ) {
        let relay_keys = mirror_keys(author);
        let tags = follows.iter().map(|pk| {
            let hex_pk = nostr_relay_builder::prelude::PublicKey::parse(&pk.to_hex())
                .expect("nmp-bdd: bridge follow pubkey across nostr crate versions");
            nostr_relay_builder::prelude::Tag::public_key(hex_pk)
        });
        let event: RelayEvent =
            RelayEventBuilder::new(nostr_relay_builder::prelude::Kind::ContactList, "")
                .tags(tags)
                .custom_created_at(nostr_relay_builder::prelude::Timestamp::from(created_at))
                .finalize(&relay_keys)
                .expect("nmp-bdd: fixture contact list must sign cleanly");
        self.relay
            .add_event(event)
            .await
            .expect("nmp-bdd: seeding a fixture contact list must succeed");
    }

    /// True iff this relay's write or query policy has been invoked at
    /// least once -- i.e. some REQ or EVENT actually reached it. The
    /// world-side half of a `must-never` "no relay outside the plan was
    /// ever contacted" assertion (see the module doc).
    pub fn contacted(&self) -> bool {
        self.contacted.load(Ordering::SeqCst) > 0
    }

    /// How many times this relay's write/query policy has been invoked --
    /// the finer-grained sibling of [`Self::contacted`], used by the
    /// "untouched" assertion (a relay whose count hasn't moved since a
    /// snapshot received no NEW REQ/EVENT at all).
    pub fn contact_count(&self) -> u64 {
        self.contacted.load(Ordering::SeqCst)
    }

    /// Stop accepting connections (used by the reconnect/drop-and-come-back
    /// scenarios: the world rebinds a fresh relay on the same port).
    pub fn shutdown(&self) {
        self.relay.shutdown();
    }
}

/// Re-derive the identical keypair under `nostr-relay-builder`'s OWN
/// (0.45-alpha) `nostr` dependency, so an event seeded directly into a
/// scripted relay is attributable to the SAME author the engine (0.44.4
/// `nostr`) knows about -- identical bridge to
/// `runtime_integration.rs::mirror_keys`.
fn mirror_keys(k: &nostr::Keys) -> RelayKeys {
    RelayKeys::parse(&k.secret_key().to_secret_hex())
        .expect("nmp-bdd: mirror keypair across nostr crate versions")
}

/// Reserve an ephemeral TCP port by binding then immediately dropping the
/// listener -- identical trick to `runtime_integration.rs::free_port`, also
/// used by the reconnect scenario to rebind the SAME port for a fresh relay
/// instance.
pub fn free_port() -> u16 {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .expect("nmp-bdd: bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

#[derive(Debug)]
struct LoggingWritePolicy {
    contacted: Arc<AtomicU64>,
    reject: bool,
}

impl WritePolicy for LoggingWritePolicy {
    fn admit_event<'a>(
        &'a self,
        _event: &'a nostr_relay_builder::prelude::Event,
        _addr: &'a SocketAddr,
    ) -> nostr_relay_builder::prelude::BoxedFuture<'a, WritePolicyResult> {
        self.contacted.fetch_add(1, Ordering::SeqCst);
        let reject = self.reject;
        Box::pin(async move {
            if reject {
                WritePolicyResult::reject(
                    MachineReadablePrefix::Error,
                    "nmp-bdd scripted relay: configured to reject every event",
                )
            } else {
                WritePolicyResult::Accept
            }
        })
    }
}

#[derive(Debug)]
struct LoggingQueryPolicy {
    contacted: Arc<AtomicU64>,
    reject: bool,
}

impl QueryPolicy for LoggingQueryPolicy {
    fn admit_query<'a>(
        &'a self,
        _query: &'a mut nostr_relay_builder::prelude::Filter,
        _addr: &'a SocketAddr,
    ) -> nostr_relay_builder::prelude::BoxedFuture<'a, QueryPolicyResult> {
        self.contacted.fetch_add(1, Ordering::SeqCst);
        let reject = self.reject;
        Box::pin(async move {
            if reject {
                QueryPolicyResult::reject(
                    MachineReadablePrefix::Error,
                    "nmp-bdd scripted relay: configured to never confirm end of stored events",
                )
            } else {
                QueryPolicyResult::Accept
            }
        })
    }
}
