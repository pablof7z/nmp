//! `NmpWorld` — one fresh world per scenario (approach doc §2.2): fresh
//! engine, fresh in-process relays, spawned lazily so every `Given` (relay
//! topology, seeded protocol state, operator config) is staged as plain data
//! before anything hits a real socket or the real `EngineThread`. The first
//! step that actually needs the engine calls [`NmpWorld::ensure_started`],
//! which starts every staged [`ScriptedRelay`], seeds their fixture events,
//! and spawns the real `nmp_engine::runtime::EngineThread` against them --
//! never a mocked engine, never a resolver-only shortcut (§2.1).
//!
//! Everything in this module is internal plumbing for the step catalog
//! (`steps::{given,when,then}`); scenarios never see it directly, and the
//! four observables it exposes (`feed_*`, `receipt_*`, `diagnostics_*`,
//! `relay_contacted`/`relay_contact_count`) are the ONLY things a `Then`
//! step is allowed to assert on (approach doc §1.3).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nostr::{EventId, Keys, PublicKey, Tag, Timestamp, UnsignedEvent};

use nmp_engine::core::{AcquisitionEvidence, DiagnosticsSnapshot, RowDelta};
use nmp_engine::outbox::{Durability, WriteIntent, WritePayload, WriteRouting, WriteStatus};
use nmp_engine::runtime::{DiagnosticsHandle, EngineThread, Handle, QueryHandle, RowsMsg};
use nmp_grammar::{Binding, Derived, Filter, IdentityField, Selector};
use nmp_resolver::LiveQuery;
use nmp_router::{Lane, LanedRelay, LiveDirectory, RelayDirectory, RelayUrl};
use nmp_signer::LocalKeySigner;
use nmp_store::MemoryStore;
use nmp_transport::PoolConfig;

use crate::relays::{RelayConfig, ScriptedRelay};

/// Bounded wait for a positive ("eventually true") assertion.
pub const EVENTUALLY: Duration = Duration::from_secs(5);
/// Bounded wait for a negative ("never becomes true") assertion -- shorter
/// on purpose (every `never` costs its FULL window, unlike `eventually`,
/// which exits early on success; keeping this small is what keeps the whole
/// suite's wall-clock bounded -- see the crate's `timeout 240` contract).
pub const NEVER: Duration = Duration::from_millis(1200);
/// Bounded wait for a relay that just came back to be recontacted by the
/// engine's own reconnect+resubscribe (#60) -- deliberately its OWN, larger
/// budget rather than reusing `EVENTUALLY` for the whole
/// reconnect-then-observe pipeline. `nmp-transport`'s `backoff::jittered`
/// adds up to 5s of per-URL deterministic jitter on top of the small
/// `reconnect_delay_initial` this world configures, and that URL always
/// contains an OS-assigned ephemeral port -- so the jitter offset silently
/// varies run to run. Folding this wait into `EVENTUALLY` (or just raising
/// `EVENTUALLY`) would still race that jitter on an unlucky port; this
/// constant instead bounds the ACTUAL reconnect signal
/// (`ScriptedRelay::wait_contacted`) with enough headroom to cover the
/// worst-case jitter, so every step AFTER "relay comes back" runs against an
/// already-reconnected relay and never has to absorb that variance itself.
pub const RECONNECT: Duration = Duration::from_secs(8);

/// The canonical name for the scenario's own (implicit "I"/"my") account --
/// every `my`/`I`-phrased step resolves through this one name, so "my
/// account" always names the same keypair as "I".
pub const ME: &str = "me";

/// The `$myFollows` shape ("my follows' notes") -- the one feed shape the
/// starter catalog names (approach doc §2.4). Identical in structure to
/// `nmp-engine`'s own `runtime_integration.rs`/`self_bootstrap_outbox.rs`
/// fixture query.
pub fn my_follows_query() -> LiveQuery {
    LiveQuery(Filter {
        kinds: Some(std::collections::BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: Filter {
                kinds: Some(std::collections::BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            },
            project: Selector::Tag("p".to_string()),
        }))),
        ..Filter::default()
    })
}

/// One accumulated feed: folds every `Added`/`Removed` delta this channel
/// has delivered so far into a live row set + the query's latest acquisition
/// evidence -- exactly what a real app must do (`Handle::subscribe`'s wire is
/// deltas, never snapshots). Persists across multiple `Then` steps in one
/// scenario.
struct FeedState {
    _handle: QueryHandle,
    rx: std::sync::mpsc::Receiver<RowsMsg>,
    rows: BTreeMap<EventId, nostr::Event>,
    evidence: AcquisitionEvidence,
}

impl FeedState {
    fn drain_available(&mut self) {
        while let Ok((deltas, evidence)) = self.rx.try_recv() {
            self.apply(deltas, evidence);
        }
    }

    fn apply(&mut self, deltas: Vec<RowDelta>, evidence: AcquisitionEvidence) {
        for delta in deltas {
            match delta {
                RowDelta::Added(event) => {
                    self.rows.insert(event.id, event);
                }
                RowDelta::Removed(id) => {
                    self.rows.remove(&id);
                }
            }
        }
        self.evidence = evidence;
    }

    /// Block (bounded) until `pred` holds against the accumulated state,
    /// draining every message that arrives in the meantime. Checks the
    /// CURRENT state first (no waiting) since a prior step's activity may
    /// already satisfy `pred`.
    fn eventually(&mut self, timeout: Duration, pred: impl Fn(&Self) -> bool) -> bool {
        self.drain_available();
        if pred(self) {
            return true;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match self.rx.recv_timeout(remaining) {
                Ok((deltas, coverage)) => {
                    self.apply(deltas, coverage);
                    if pred(self) {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
    }

    /// Block the FULL window, returning `true` iff `pred` never held at any
    /// point during it (the settle-window half of "never" -- approach doc
    /// §1.3).
    fn never(&mut self, timeout: Duration, pred: impl Fn(&Self) -> bool) -> bool {
        self.drain_available();
        if pred(self) {
            return false;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            match self.rx.recv_timeout(remaining) {
                Ok((deltas, coverage)) => {
                    self.apply(deltas, coverage);
                    if pred(self) {
                        return false;
                    }
                }
                Err(_) => return true,
            }
        }
    }
}

/// The receipt stream for the most recent `publish` (the starter catalog
/// only ever names "the receipt", singular -- one implicit publish in
/// flight per scenario).
struct ReceiptState {
    rx: std::sync::mpsc::Receiver<WriteStatus>,
    seen: Vec<WriteStatus>,
}

impl ReceiptState {
    fn drain_available(&mut self) {
        while let Ok(status) = self.rx.try_recv() {
            self.seen.push(status);
        }
    }

    fn eventually(&mut self, timeout: Duration, pred: impl Fn(&[WriteStatus]) -> bool) -> bool {
        self.drain_available();
        if pred(&self.seen) {
            return true;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(status) => {
                    self.seen.push(status);
                    if pred(&self.seen) {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
    }
}

/// Forwards the single-slot `LatestReceiver<DiagnosticsSnapshot>` into a
/// `Condvar`-signalled shared slot so callers can `wait_timeout` on it (the
/// runtime's own `LatestReceiver::recv` has no timeout variant -- see its
/// doc; this is the same "latest wins" idea, made boundedly pollable).
struct DiagFeed {
    _handle: DiagnosticsHandle,
    shared: Arc<(Mutex<Option<DiagnosticsSnapshot>>, Condvar)>,
    _forwarder: JoinHandle<()>,
}

impl DiagFeed {
    fn new(
        handle: DiagnosticsHandle,
        rx: nmp_engine::runtime::LatestReceiver<DiagnosticsSnapshot>,
    ) -> Self {
        let shared = Arc::new((Mutex::new(None), Condvar::new()));
        let shared2 = Arc::clone(&shared);
        let forwarder = thread::spawn(move || {
            while let Some(snapshot) = rx.recv() {
                let (lock, cvar) = &*shared2;
                *lock.lock().expect("nmp-bdd: diagnostics forwarder lock") = Some(snapshot);
                cvar.notify_all();
            }
        });
        Self {
            _handle: handle,
            shared,
            _forwarder: forwarder,
        }
    }

    /// Block (bounded) until `pred` holds against the latest snapshot,
    /// returning the snapshot that satisfied it (`None` on timeout).
    fn get(
        &self,
        timeout: Duration,
        pred: impl Fn(&DiagnosticsSnapshot) -> bool,
    ) -> Option<DiagnosticsSnapshot> {
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().expect("nmp-bdd: diagnostics lock");
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(snapshot) = guard.as_ref() {
                if pred(snapshot) {
                    return Some(snapshot.clone());
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (g, _timeout) = cvar
                .wait_timeout(guard, remaining)
                .expect("nmp-bdd: diagnostics wait");
            guard = g;
        }
    }
}

/// Everything staged for a person's kind:3 (contact list): who, whom they
/// follow, when. Resolved to actual relays only at `ensure_started` (kind:3
/// is a discovery-kind atom -- it is seeded at every configured indexer PLUS
/// every relay the author's own declared write-relay list names, if any;
/// see that method).
struct PendingContactList {
    author: String,
    follows: Vec<String>,
    created_at: u64,
}

/// A staged kind:1 note: author, text, when. Seeded at every one of the
/// author's own declared write relays -- content atoms never route to an
/// indexer, so a note is never findable anywhere else.
struct PendingNote {
    author: String,
    text: String,
    created_at: u64,
}

#[derive(cucumber::World, Default)]
pub struct NmpWorld {
    people: HashMap<String, Keys>,

    relay_configs: HashMap<String, RelayConfig>,
    relay_order: Vec<String>,
    relays: HashMap<String, ScriptedRelay>,
    indexer_names: Vec<String>,
    write_relay_of: HashMap<String, Vec<String>>,

    pending_contact_lists: Vec<PendingContactList>,
    pending_notes: Vec<PendingNote>,

    active_person: Option<String>,
    ts_counter: u64,
    switch_counter: u64,
    started: bool,

    engine: Option<EngineThread>,
    handle: Option<Handle>,
    feed: Option<FeedState>,
    last_receipt: Option<ReceiptState>,
    diag: Option<DiagFeed>,
    contact_snapshot: HashMap<String, u64>,
}

impl std::fmt::Debug for NmpWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NmpWorld")
            .field("people", &self.people.keys().collect::<Vec<_>>())
            .field("relays", &self.relay_order)
            .field("indexers", &self.indexer_names)
            .field("active_person", &self.active_person)
            .field("started", &self.started)
            .field("feed_open", &self.feed.is_some())
            .finish()
    }
}

impl NmpWorld {
    // ---- Given-time staging (no I/O yet) -------------------------------

    /// Mint-or-get a named fixture keypair. Every person mentioned anywhere
    /// in a scenario resolves through this single method, so "Alice" always
    /// names the same keypair for the rest of that scenario.
    pub fn person(&mut self, name: &str) -> Keys {
        self.people
            .entry(name.to_string())
            .or_insert_with(Keys::generate)
            .clone()
    }

    fn relay_config_mut(&mut self, name: &str) -> &mut RelayConfig {
        if !self.relay_configs.contains_key(name) {
            self.relay_order.push(name.to_string());
        }
        self.relay_configs.entry(name.to_string()).or_default()
    }

    /// `Given a relay <name> exists (that nothing references)` -- registers
    /// it with default (well-behaved) config and no role at all.
    pub fn register_bystander_relay(&mut self, name: &str) {
        self.relay_config_mut(name);
    }

    /// `Given only <n> indexer relays are configured` -- mints `n` relays
    /// named `indexer-1..indexer-n`.
    pub fn configure_n_indexers(&mut self, n: usize) {
        for i in 1..=n {
            let name = format!("indexer-{i}");
            self.relay_config_mut(&name);
            if !self.indexer_names.contains(&name) {
                self.indexer_names.push(name);
            }
        }
    }

    /// `Given relays <list> are configured as indexers`.
    pub fn configure_named_indexers(&mut self, names: &[String]) {
        for name in names {
            self.relay_config_mut(name);
            if !self.indexer_names.contains(name) {
                self.indexer_names.push(name.clone());
            }
        }
    }

    pub fn set_reject_writes(&mut self, relay: &str) {
        self.relay_config_mut(relay).reject_writes = true;
    }

    pub fn set_reject_queries(&mut self, relay: &str) {
        self.relay_config_mut(relay).reject_queries = true;
    }

    /// `Given <person>'s relay list names <relay> as their write relay` /
    /// `Given my relay list names <relay>[s] as my write relay(s)`.
    pub fn declare_write_relay(&mut self, person: &str, relay: &str) {
        self.person(person);
        self.relay_config_mut(relay);
        let relays = self.write_relay_of.entry(person.to_string()).or_default();
        if !relays.iter().any(|r| r == relay) {
            relays.push(relay.to_string());
        }
    }

    /// The relay `person`'s own content is written to -- their FIRST
    /// declared write relay if `Given` any, otherwise a fresh
    /// `"<person>-relay"` auto-registered on first use (a scenario that
    /// never bothers staging an explicit relay list still gets a perfectly
    /// normal single-relay world).
    fn write_relay_for(&mut self, person: &str) -> String {
        if let Some(existing) = self.write_relay_of.get(person).and_then(|v| v.first()) {
            return existing.clone();
        }
        let name = format!("{person}-relay");
        self.declare_write_relay(person, &name);
        name
    }

    fn next_created_at(&mut self) -> u64 {
        self.ts_counter += 1;
        1_700_000_000 + self.ts_counter
    }

    /// `Given <person> follows <people>` / the login shape's own implicit
    /// contact list.
    pub fn stage_follows(&mut self, person: &str, follows: &[String]) {
        self.person(person);
        for f in follows {
            self.person(f);
        }
        let created_at = self.next_created_at();
        self.pending_contact_lists.push(PendingContactList {
            author: person.to_string(),
            follows: follows.to_vec(),
            created_at,
        });
    }

    /// `Given I am logged in as an account that follows <people>` /
    /// `Given I am logged in as my own account` / `Given I am logged in as
    /// <person>'s account`.
    pub fn log_in_as(&mut self, person: &str, follows: &[String]) {
        self.person(person);
        self.active_person = Some(person.to_string());
        if !follows.is_empty() {
            self.stage_follows(person, follows);
        }
    }

    /// `Given <person> has posted <n> notes` / `a note saying <text>`.
    pub fn stage_note(&mut self, person: &str, text: &str) {
        self.person(person);
        self.write_relay_for(person);
        let created_at = self.next_created_at();
        self.pending_notes.push(PendingNote {
            author: person.to_string(),
            text: text.to_string(),
            created_at,
        });
    }

    // ---- lazy startup ----------------------------------------------------

    /// Start every staged relay, seed every staged fixture event, and spawn
    /// the REAL `EngineThread` against them -- idempotent, called by every
    /// step that actually touches the engine or a relay (approach doc §2.2:
    /// "spawned lazily on the first `When`").
    pub async fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.started = true;

        for name in self.relay_order.clone() {
            let config = self.relay_configs.get(&name).cloned().unwrap_or_default();
            let relay = ScriptedRelay::start(&config).await;
            self.relays.insert(name, relay);
        }

        let indexer_urls: Vec<RelayUrl> = self
            .indexer_names
            .iter()
            .map(|name| self.relays[name].url.clone())
            .collect();
        let mut directory = LiveDirectory::builder().indexers(indexer_urls).build();
        for (person, relay_names) in self.write_relay_of.clone() {
            let pk_hex = self.person(&person).public_key().to_hex();
            let laned: Vec<LanedRelay> = relay_names
                .iter()
                .map(|name| LanedRelay::new(self.relays[name].url.clone(), Lane::Nip65Write))
                .collect();
            directory.ingest_write_relays(pk_hex, laned);
        }

        for pending in std::mem::take(&mut self.pending_contact_lists) {
            let author_keys = self.person(&pending.author);
            let follow_pks: Vec<PublicKey> = pending
                .follows
                .iter()
                .map(|name| self.person(name).public_key())
                .collect();
            let mut targets: Vec<String> = self.indexer_names.clone();
            if let Some(own) = self.write_relay_of.get(&pending.author) {
                for r in own {
                    if !targets.contains(r) {
                        targets.push(r.clone());
                    }
                }
            }
            for relay_name in targets {
                self.relays[&relay_name]
                    .seed_contact_list(&author_keys, &follow_pks, pending.created_at)
                    .await;
            }
        }

        for pending in std::mem::take(&mut self.pending_notes) {
            let author_keys = self.person(&pending.author);
            let relay_names = self
                .write_relay_of
                .get(&pending.author)
                .cloned()
                .expect("nmp-bdd: a staged note's author must already have a write relay");
            for relay_name in relay_names {
                self.relays[&relay_name]
                    .seed_note(&author_keys, &pending.text, pending.created_at)
                    .await;
            }
        }

        let (engine_thread, handle) = EngineThread::spawn(
            MemoryStore::new(),
            directory,
            20,
            PoolConfig {
                reconnect_delay_initial: Some(Duration::from_millis(20)),
                ..PoolConfig::default()
            },
        );

        if let Some(active) = self.active_person.clone() {
            let keys = self.person(&active);
            handle.add_signer(LocalKeySigner::new(keys.clone()));
            handle.set_active_account(Some(keys.public_key()));
        }

        let (diag_handle, diag_rx) = handle.observe_diagnostics();
        self.diag = Some(DiagFeed::new(diag_handle, diag_rx));

        self.engine = Some(engine_thread);
        self.handle = Some(handle);
    }

    fn handle(&self) -> &Handle {
        self.handle
            .as_ref()
            .expect("nmp-bdd: the engine must be started (ensure_started) before use")
    }

    // ---- When actions -----------------------------------------------------

    /// `When I open a feed of my follows' notes` / the `Given` shorthand
    /// `my feed of my follows' notes is open`.
    pub async fn open_my_follows_feed(&mut self) {
        self.ensure_started().await;
        let (handle_id, rx) = self.handle().subscribe(my_follows_query());
        self.feed = Some(FeedState {
            _handle: handle_id,
            rx,
            rows: BTreeMap::new(),
            evidence: AcquisitionEvidence::default(),
        });
    }

    /// `When I publish a new follow list with <people>`.
    pub async fn publish_new_follow_list(&mut self, people: &[String]) {
        self.ensure_started().await;
        self.snapshot_relay_contacts();
        let me = self
            .active_person
            .clone()
            .expect("nmp-bdd: publishing a follow list needs a logged-in account");
        let me_keys = self.person(&me);
        // Mint any name mentioned here for the first time (e.g. a fresh
        // follow the scenario never staged as a `Given`) before building
        // tags -- never index `self.people` directly, which would panic on
        // an unminted name.
        let follow_pks: Vec<PublicKey> = people
            .iter()
            .map(|name| self.person(name).public_key())
            .collect();
        let tags: Vec<Tag> = follow_pks.into_iter().map(Tag::public_key).collect();
        let unsigned = UnsignedEvent::new(
            me_keys.public_key(),
            Timestamp::now(),
            nostr::Kind::ContactList,
            tags,
            "",
        );
        let rx = self
            .handle()
            .publish(WriteIntent {
                payload: WritePayload::Unsigned(unsigned),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
            })
            .expect("BDD receipt correlation namespace must be available");
        self.last_receipt = Some(ReceiptState {
            rx,
            seen: Vec::new(),
        });
    }

    /// `When I publish a note saying <text>`.
    pub async fn publish_note(&mut self, text: &str) {
        self.ensure_started().await;
        let me = self
            .active_person
            .clone()
            .expect("nmp-bdd: publishing a note needs a logged-in account");
        let me_keys = self.person(&me);
        let unsigned = UnsignedEvent::new(
            me_keys.public_key(),
            Timestamp::now(),
            nostr::Kind::TextNote,
            vec![],
            text,
        );
        let rx = self
            .handle()
            .publish(WriteIntent {
                payload: WritePayload::Unsigned(unsigned),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
            })
            .expect("BDD receipt correlation namespace must be available");
        self.last_receipt = Some(ReceiptState {
            rx,
            seen: Vec::new(),
        });
    }

    /// `When I switch to <person>'s account` (a person already known to the
    /// world, e.g. previously logged in as).
    pub async fn switch_account(&mut self, person: &str) {
        self.ensure_started().await;
        let keys = self.person(person);
        self.handle().add_signer(LocalKeySigner::new(keys.clone()));
        self.handle().set_active_account(Some(keys.public_key()));
        self.active_person = Some(person.to_string());
    }

    /// `When I switch to a new account that follows <people>` -- mints a
    /// brand-new identity never seen before this point in the scenario, so
    /// an account-switch scenario doesn't need every future account
    /// pre-declared as a `Given`. Seeds the new account's kind:3 directly at
    /// every configured indexer (kind:3 is a discovery atom, and this
    /// pubkey has never been resolved before, so that's exactly where the
    /// engine's freshly re-rooted discovery REQ will look) BEFORE flipping
    /// the active account, so the backlog is already there when the engine
    /// asks.
    pub async fn switch_to_new_account_following(&mut self, follows: &[String]) {
        self.ensure_started().await;
        self.switch_counter += 1;
        let name = format!("switched-account-{}", self.switch_counter);
        let keys = self.person(&name);
        let follow_pks: Vec<PublicKey> = follows
            .iter()
            .map(|f| self.person(f).public_key())
            .collect();
        let created_at = self.next_created_at();
        for indexer in self.indexer_names.clone() {
            self.relays[&indexer]
                .seed_contact_list(&keys, &follow_pks, created_at)
                .await;
        }
        self.handle().add_signer(LocalKeySigner::new(keys.clone()));
        self.handle().set_active_account(Some(keys.public_key()));
        self.active_person = Some(name);
    }

    /// `When <person> posts a note saying <text>` -- a LIVE event from
    /// ANOTHER user (never routed through this world's own `Handle`, which
    /// only ever signs for the active/logged-in account): seeded directly
    /// into `person`'s own write relay, exactly as a real relay would
    /// receive and store it the instant it was published elsewhere.
    pub async fn person_posts_note_live(&mut self, person: &str, text: &str) {
        self.ensure_started().await;
        let keys = self.person(person);
        let relay_name = self
            .write_relay_of
            .get(person)
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_else(|| panic!("nmp-bdd: {person} has no write relay to post to"));
        let created_at = self.next_created_at();
        self.relays[&relay_name]
            .seed_note(&keys, text, created_at)
            .await;
    }

    /// `When relay <name> drops the connection`.
    pub fn drop_relay_connection(&mut self, name: &str) {
        self.relays[name].shutdown();
    }

    /// `When relay <name> comes back` -- rebinds a fresh `LocalRelay` on the
    /// exact same port (see `ScriptedRelay::start_on_port`'s doc), so the
    /// engine's own `Pool` reconnects to the SAME `RelayUrl` it already had
    /// open and replays its current subscriptions there with no
    /// resubscribe.
    ///
    /// Does NOT return until that reconnect+resubscribe has actually
    /// happened (#60): the fresh instance's own `ContactLog` starts at zero,
    /// so `wait_contacted` blocks (bounded by `RECONNECT`, no spin-poll) for
    /// its first REQ/EVENT -- concrete proof the `Pool` is back, rather than
    /// letting this step return immediately and leaving every later step
    /// (`Bob posts a note`, `Then my feed shows ...`) to absorb whatever's
    /// left of the reconnect's own (jittered, run-to-run-varying) backoff
    /// delay out of ITS fixed window. See `RECONNECT`'s doc for why this is
    /// a dedicated budget, not a bigger `EVENTUALLY`.
    pub async fn relay_comes_back(&mut self, name: &str) {
        let port = self.relays[name].port();
        let config = self.relay_configs.get(name).cloned().unwrap_or_default();
        // Give the OS a moment to release the port -- identical wait to
        // `runtime_integration.rs`'s own reconnect half.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let fresh = ScriptedRelay::start_on_port(port, &config).await;
        assert!(
            fresh.wait_contacted(RECONNECT).await,
            "nmp-bdd: relay {name:?} was not recontacted within {RECONNECT:?} of coming back -- \
             the engine's Pool did not reconnect/resubscribe in time"
        );
        self.relays.insert(name.to_string(), fresh);
    }

    // ---- Then observables ---------------------------------------------

    pub fn feed_eventually(
        &mut self,
        pred: impl Fn(&[nostr::Event], &AcquisitionEvidence) -> bool,
    ) -> bool {
        let feed = self.feed.as_mut().expect("nmp-bdd: no feed is open");
        feed.eventually(EVENTUALLY, |f| {
            let rows: Vec<nostr::Event> = f.rows.values().cloned().collect();
            pred(&rows, &f.evidence)
        })
    }

    pub fn feed_never(&mut self, pred: impl Fn(&[nostr::Event]) -> bool) -> bool {
        let feed = self.feed.as_mut().expect("nmp-bdd: no feed is open");
        feed.never(NEVER, |f| {
            let rows: Vec<nostr::Event> = f.rows.values().cloned().collect();
            pred(&rows)
        })
    }

    pub fn receipt_eventually(&mut self, pred: impl Fn(&[WriteStatus]) -> bool) -> bool {
        let receipt = self
            .last_receipt
            .as_mut()
            .expect("nmp-bdd: no publish is in flight");
        receipt.eventually(EVENTUALLY, pred)
    }

    pub fn diagnostics_matching(
        &self,
        pred: impl Fn(&DiagnosticsSnapshot) -> bool,
    ) -> Option<DiagnosticsSnapshot> {
        let diag = self.diag.as_ref().expect("nmp-bdd: diagnostics not open");
        diag.get(EVENTUALLY, pred)
    }

    /// True iff the named relay's write/query policy has ever been invoked
    /// -- the world-side "was this relay ever contacted" observable
    /// (independent of the engine's own diagnostics self-report; see
    /// `relays.rs`'s doc).
    pub fn relay_contacted(&self, name: &str) -> bool {
        self.relays
            .get(name)
            .map(ScriptedRelay::contacted)
            .unwrap_or(false)
    }

    /// Record the CURRENT contact-count of every known relay -- the
    /// "before" half of an "untouched since this point" assertion.
    fn snapshot_relay_contacts(&mut self) {
        self.contact_snapshot = self
            .relays
            .iter()
            .map(|(name, r)| (name.clone(), r.contact_count()))
            .collect();
    }

    /// True iff `name`'s contact-count is EXACTLY what it was at the last
    /// [`Self::snapshot_relay_contacts`] call -- i.e. no NEW REQ/EVENT ever
    /// reached it since then (the "untouched" `Then`).
    pub fn relay_untouched_since_snapshot(&self, name: &str) -> bool {
        let before = self.contact_snapshot.get(name).copied().unwrap_or(0);
        let after = self
            .relays
            .get(name)
            .map(ScriptedRelay::contact_count)
            .unwrap_or(0);
        before == after
    }

    pub fn indexer_names(&self) -> &[String] {
        &self.indexer_names
    }

    pub fn relay_names(&self) -> impl Iterator<Item = &String> {
        self.relay_order.iter()
    }

    pub fn relay_url(&self, name: &str) -> RelayUrl {
        self.relays
            .get(name)
            .unwrap_or_else(|| panic!("nmp-bdd: unknown relay {name:?}"))
            .url
            .clone()
    }

    pub fn write_relay_of(&self, person: &str) -> Vec<String> {
        self.write_relay_of.get(person).cloned().unwrap_or_default()
    }

    pub fn pubkey_hex(&self, person: &str) -> String {
        self.people
            .get(person)
            .unwrap_or_else(|| panic!("nmp-bdd: unknown person {person:?}"))
            .public_key()
            .to_hex()
    }
}
