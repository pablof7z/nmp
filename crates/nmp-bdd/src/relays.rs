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
//!   (`ScriptedRelay::contacted`/`contact_count`/`wait_contacted`), bumped by
//!   those SAME policy hooks on every inbound EVENT/REQ. This is
//!   deliberately independent of the engine's own `DiagnosticsSnapshot`: a
//!   `must-never` scenario asserting "no relay outside the plan was ever
//!   contacted" must not take the engine's self-report as its only witness,
//!   or a diagnostics bug could silently make the ledger scenario
//!   un-falsifiable. `wait_contacted` is the same log's BOUNDED-WAIT half
//!   (#60): a freshly rebound relay instance (`start_on_port`, used by the
//!   reconnect scenario) starts its own count at zero, so blocking on its
//!   first contact is a deterministic "the engine's `Pool` reconnected and
//!   resubscribed here" signal, rather than a fixed-timeout guess racing an
//!   unrelated, run-to-run-varying backoff delay.
//!
//! Deliberately NOT a glob import of `nostr_relay_builder::prelude::*` in
//! the signature-facing parts of this module: that re-exports a DIFFERENT
//! `nostr` (0.45-alpha) than this workspace's pinned `nostr = "0.44.4"` --
//! see `nmp-engine/tests/runtime_integration.rs`'s identical comment. Every
//! cross-version value (keypairs, seeded events) is bridged explicitly by
//! hex/id string round-trip (`mirror_keys`).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use nostr::{JsonUtil, RelayUrl};

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

/// Contact count + a `Notify` so a caller can WAIT (bounded, no spin-poll --
/// same idiom as `NmpWorld`'s `FeedState`/`ReceiptState`/`DiagFeed`) for the
/// NEXT contact rather than guessing how long one takes to arrive. This is
/// what makes the reconnect scenario's "relay comes back" step deterministic
/// (#60): instead of assuming the engine's `Pool` has already reconnected and
/// resubscribed by some fixed wall-clock offset, the world can wait for THIS
/// relay instance's OWN evidence that a REQ/EVENT actually reached it again.
#[derive(Debug, Default)]
struct ContactLog {
    count: AtomicU64,
    notify: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct QueryLog {
    by_kind: Mutex<BTreeMap<u16, u64>>,
}

impl QueryLog {
    fn record(&self, query: &nostr_relay_builder::prelude::Filter) {
        let value = serde_json::to_value(query)
            .expect("nmp-bdd: scripted relay query must serialize for observation");
        let Some(kinds) = value.get("kinds").and_then(serde_json::Value::as_array) else {
            return;
        };
        let mut counts = self
            .by_kind
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for kind in kinds.iter().filter_map(serde_json::Value::as_u64) {
            *counts.entry(kind as u16).or_default() += 1;
        }
    }

    fn count(&self, kind: u16) -> u64 {
        self.by_kind
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(&kind)
            .copied()
            .unwrap_or(0)
    }
}

impl ContactLog {
    fn record(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn count(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }

    /// Bounded wait for the count to become nonzero -- never a spin-poll
    /// loop. Just pinning `Notify::notified()`'s returned future does NOT
    /// yet register it as a waiter (that only happens once the future is
    /// polled), so a `record()` -> `notify_waiters()` racing between the
    /// second `count()` check and this future's first poll would otherwise
    /// notify zero waiters and be silently lost -- the classic `Notify`
    /// lost-wakeup trap. `Notified::enable()` is Tokio's documented fix:
    /// it registers the waiter immediately, without consuming a poll, so
    /// calling it BEFORE the second check closes that exact window.
    async fn wait_contacted(&self, timeout: Duration) -> bool {
        if self.count() > 0 {
            return true;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.count() > 0 {
            return true;
        }
        tokio::time::timeout(timeout, notified).await.is_ok()
    }
}

/// One running in-process relay + its contacted-log.
pub struct ScriptedRelay {
    pub url: RelayUrl,
    port: u16,
    relay: LocalRelay,
    contacted: Arc<ContactLog>,
    queries: Arc<QueryLog>,
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
        let contacted = Arc::new(ContactLog::default());
        let queries = Arc::new(QueryLog::default());

        let relay = LocalRelayBuilder::default()
            .addr(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .port(port)
            .write_policy(LoggingWritePolicy {
                contacted: contacted.clone(),
                reject: config.reject_writes,
            })
            .query_policy(LoggingQueryPolicy {
                contacted: contacted.clone(),
                queries: queries.clone(),
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
            queries,
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

    /// Seed an already-signed workspace event verbatim into this relay.
    /// JSON round-tripping bridges the two pinned `nostr` crate versions
    /// without re-signing, which lets product-level parity tests compare
    /// every raw row token (including the signature) exactly.
    pub async fn seed_signed_event(&self, event: &nostr::Event) {
        let event: RelayEvent = serde_json::from_str(&event.as_json())
            .expect("nmp-bdd: bridge signed fixture across nostr crate versions");
        self.relay
            .add_event(event)
            .await
            .expect("nmp-bdd: seeding a fixture event must succeed");
    }

    /// Seed the author's NIP-65 relay list, naming this relay as an
    /// unmarked read+write relay. The facade parity scenario uses the real
    /// discovery path before publishing; it never injects a mechanism-level
    /// directory fixture into either product surface.
    pub async fn seed_own_relay_list(&self, author: &nostr::Keys, created_at: u64) {
        let relay_keys = mirror_keys(author);
        let relay_url = self.url.to_string();
        let event = RelayEventBuilder::new(nostr_relay_builder::prelude::Kind::RelayList, "")
            .tag(
                nostr_relay_builder::prelude::Tag::parse(["r".to_string(), relay_url])
                    .expect("nmp-bdd: relay-list fixture tag must parse"),
            )
            .custom_created_at(nostr_relay_builder::prelude::Timestamp::from(created_at))
            .finalize(&relay_keys)
            .expect("nmp-bdd: relay-list fixture must sign cleanly");
        self.relay
            .add_event(event)
            .await
            .expect("nmp-bdd: seeding a relay-list fixture must succeed");
    }

    /// True iff this relay's write or query policy has been invoked at
    /// least once -- i.e. some REQ or EVENT actually reached it. The
    /// world-side half of a `must-never` "no relay outside the plan was
    /// ever contacted" assertion (see the module doc).
    pub fn contacted(&self) -> bool {
        self.contacted.count() > 0
    }

    /// How many times this relay's write/query policy has been invoked --
    /// the finer-grained sibling of [`Self::contacted`], used by the
    /// "untouched" assertion (a relay whose count hasn't moved since a
    /// snapshot received no NEW REQ/EVENT at all).
    pub fn contact_count(&self) -> u64 {
        self.contacted.count()
    }

    /// Number of inbound REQs admitted to this relay's `QueryPolicy` whose
    /// filter named `kind` (whether the policy later accepts or rejects the
    /// query). This relay-side admission witness is independent of engine
    /// diagnostics; parity tests conjunct it with engine event counts to
    /// prove every admitted fixture response was processed before advancing.
    pub fn query_count_for_kind(&self, kind: u16) -> u64 {
        self.queries.count(kind)
    }

    /// Bounded wait (no spin-poll -- see [`ContactLog::wait_contacted`]) for
    /// this relay instance to be contacted at least once. The deterministic
    /// "has the engine's `Pool` actually reconnected and resubscribed here
    /// yet" signal the reconnect scenario needs (#60): a freshly rebound
    /// instance's `ContactLog` starts at zero, so the first contact IS the
    /// resubscribe-after-reconnect event, whenever it actually lands --
    /// never a guess at how long that takes.
    pub async fn wait_contacted(&self, timeout: Duration) -> bool {
        self.contacted.wait_contacted(timeout).await
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
    contacted: Arc<ContactLog>,
    reject: bool,
}

impl WritePolicy for LoggingWritePolicy {
    fn admit_event<'a>(
        &'a self,
        _event: &'a nostr_relay_builder::prelude::Event,
        _addr: &'a SocketAddr,
    ) -> nostr_relay_builder::prelude::BoxedFuture<'a, WritePolicyResult> {
        self.contacted.record();
        let reject = self.reject;
        Box::pin(async move {
            if reject {
                WritePolicyResult::reject(
                    MachineReadablePrefix::Blocked,
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
    contacted: Arc<ContactLog>,
    queries: Arc<QueryLog>,
    reject: bool,
}

impl QueryPolicy for LoggingQueryPolicy {
    fn admit_query<'a>(
        &'a self,
        query: &'a mut nostr_relay_builder::prelude::Filter,
        _addr: &'a SocketAddr,
    ) -> nostr_relay_builder::prelude::BoxedFuture<'a, QueryPolicyResult> {
        self.contacted.record();
        self.queries.record(query);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Falsifier for the lost-wakeup trap `wait_contacted` must close
    /// (caught pre-merge on #60/PR #72 by codex-nova review): a bare
    /// `tokio::pin!(notify.notified())` does NOT register the future as a
    /// waiter -- only polling (or `enable()`) does -- so a `record()`
    /// landing between the "already contacted?" recheck and this future's
    /// first poll would notify zero waiters under `notify_waiters()` and be
    /// silently dropped. This pins the actual contract `wait_contacted`
    /// relies on: call `enable()` first, and a contact recorded after that
    /// point but strictly before the future is ever polled must still be
    /// observed.
    #[tokio::test]
    async fn enabled_notified_future_observes_a_contact_recorded_before_its_first_poll() {
        let log = ContactLog::default();
        let notified = log.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        // Exactly the ordering `wait_contacted`'s internal race window
        // allows: contact recorded after `enable()`, strictly before the
        // future's first poll (this test never polled it before this line).
        log.record();

        tokio::time::timeout(Duration::from_millis(200), notified)
            .await
            .expect(
                "an enable()d waiter must still observe a contact recorded before its first poll",
            );
    }

    /// End-to-end sibling: `wait_contacted` itself must observe a
    /// GENUINELY concurrent `record()` (a second task on a real
    /// multi-thread runtime, not this same synchronous sequence) --
    /// repeated many times to build confidence against scheduler-dependent
    /// flakiness, the same reason #60's own reconnect fix was proven 5x
    /// back-to-back rather than once. Would have been flaky/failing
    /// intermittently against the pre-`enable()` version of `wait_contacted`.
    #[tokio::test(flavor = "multi_thread")]
    async fn wait_contacted_observes_a_concurrent_record_under_repeated_trials() {
        for _ in 0..200 {
            let log = Arc::new(ContactLog::default());
            let recorder = {
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    log.record();
                })
            };
            let seen = log.wait_contacted(Duration::from_millis(500)).await;
            recorder.await.expect("recorder task must not panic");
            assert!(seen, "wait_contacted missed a concurrent record()");
        }
    }
}
