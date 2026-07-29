//! `Given`-time staging, and the single lazy start that consumes it.
//!
//! Everything here runs BEFORE anything is listening: a `Given` records plain
//! data (a relay's name and policy, who follows whom, what has been posted)
//! and touches no socket. That is the whole reason this is one module rather
//! than two -- the staged fields and [`NmpWorld::ensure_started`] are a
//! producer and its only consumer, and the rules for how staged data becomes
//! real (a kind:3 goes to every indexer PLUS the author's own write relays; a
//! kind:1 goes only to the author's write relays) are unreadable apart from
//! the staging that feeds them.

use std::time::Duration;

use nostr::{Keys, PublicKey, Timestamp, UnsignedEvent};

use nmp::mechanism::runtime::{EngineThread, Handle};
use nmp_router::{Lane, LanedRelay, LiveDirectory, RelayDirectory, RelayUrl};
use nmp_store::{EventStore, MemoryStore, RedbStore};
use nmp_transport::PoolConfig;

use nmp_test_support::relays::{RelayConfig, ScriptedRelay};

use super::observe::DiagFeed;
use super::NmpWorld;

/// Everything staged for a person's kind:3 (contact list): who, whom they
/// follow, when. Resolved to actual relays only at `ensure_started` (kind:3
/// is a discovery-kind atom -- it is seeded at every configured indexer PLUS
/// every relay the author's own declared write-relay list names, if any;
/// see that method).
pub(super) struct PendingContactList {
    author: String,
    follows: Vec<String>,
    created_at: u64,
}

/// A staged kind:1 note: author, text, when. Seeded at every one of the
/// author's own declared write relays -- content atoms never route to an
/// indexer, so a note is never findable anywhere else.
pub(super) struct PendingNote {
    author: String,
    text: String,
    created_at: u64,
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

    pub(super) fn relay_config_mut(&mut self, name: &str) -> &mut RelayConfig {
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

    /// `Given relay <name> allows at most <n> subscriptions at a time` --
    /// the relay publishes a NIP-11 document saying so, served over plain
    /// HTTP on its own address exactly as a real relay does. The engine
    /// fetches and parses it through its own acquisition path; nothing is
    /// injected behind its back.
    pub fn advertise_subscription_limit(&mut self, relay: &str, max: u64) {
        self.relay_config_mut(relay)
            .advertised_limits
            .get_or_insert_with(Default::default)
            .max_subscriptions = Some(max);
    }

    /// `Given relay <name> accepts subscription names of at most <n>
    /// characters`.
    pub fn advertise_subid_length(&mut self, relay: &str, max: u64) {
        self.relay_config_mut(relay)
            .advertised_limits
            .get_or_insert_with(Default::default)
            .max_subid_length = Some(max);
    }

    /// `Given relay <name> publishes nothing about itself` -- no NIP-11
    /// document at all, answered `404`. This is the DEFAULT for every relay
    /// in this suite, stated explicitly where a scenario is about it: two of
    /// the eight major public relays measured for issue #931 behave this way.
    pub fn publish_no_relay_document(&mut self, relay: &str) {
        self.relay_config_mut(relay).advertised_limits = None;
    }

    /// `Given <person>'s relay list names <relay> as their write relay` /
    /// `Given my relay list names <relay>[s] as my write relay(s)`.
    ///
    /// Declaring MINE also declares it for every identity an identity
    /// scenario registered: `features/identity/` states the relay list once,
    /// as an app owner would, and then has several identities this one user
    /// holds publish through it. See `identity::propagate_my_write_relay`.
    pub fn declare_write_relay(&mut self, person: &str, relay: &str) {
        self.person(person);
        self.relay_config_mut(relay);
        let relays = self.write_relay_of.entry(person.to_string()).or_default();
        if !relays.iter().any(|r| r == relay) {
            relays.push(relay.to_string());
        }
        if person == super::ME {
            self.propagate_my_write_relay(relay);
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

    pub(super) fn next_created_at(&mut self) -> u64 {
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

    /// The same staging, but the world KEEPS the exact signed event so a
    /// later step can republish it verbatim. Routing and authorship are
    /// separate axes: republishing this event is the standing proof, and it
    /// only proves anything if the bytes that go back out are the bytes
    /// their author signed.
    pub fn stage_signed_note(&mut self, person: &str, text: &str) {
        let keys = self.person(person);
        self.write_relay_for(person);
        let created_at = self.next_created_at();
        let signed = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(created_at),
            nostr::Kind::TextNote,
            vec![],
            text,
        )
        .sign_with_keys(&keys)
        .expect("fixture keys sign cleanly");
        self.signed_notes.insert(text.to_string(), signed.clone());
        self.pending_signed_notes.push((person.to_string(), signed));
    }

    /// This world configures no app relays anywhere -- the `Given` that says
    /// so out loud is checking the harness, not the engine.
    pub fn assert_no_app_relays(&self) {
        assert!(
            !self.started,
            "nmp-bdd: state the app-relay topology before anything runs"
        );
    }

    /// Logging in registers a real signer (see `ensure_started`), so a
    /// scenario whose point is "the signer was never asked" only means
    /// something once one exists to ask.
    pub fn assert_signer_registered(&self) {
        assert!(
            self.active_person.is_some(),
            "nmp-bdd: a registered signer needs a logged-in account"
        );
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

        for (author, event) in std::mem::take(&mut self.pending_signed_notes) {
            let relay_names = self
                .write_relay_of
                .get(&author)
                .cloned()
                .expect("nmp-bdd: a staged note's author must already have a write relay");
            for relay_name in relay_names {
                self.relays[&relay_name].seed_signed_event(&event).await;
            }
        }

        for group in std::mem::take(&mut self.pending_groups) {
            self.seed_group_admins(&group).await;
        }

        self.spawn_engine().await;
    }

    /// Build the directory from the staged relay topology and spawn a REAL
    /// `EngineThread` over this scenario's durable store.
    ///
    /// Its own method, called by `ensure_started` and again by the identity
    /// plane's restart step, because "reconstruct the engine from the same
    /// durable store" only means anything if the second engine is built the
    /// same way the first was -- same directory facts, same relays, same
    /// admission policy, nothing carried over in memory.
    pub(super) async fn spawn_engine(&mut self) {
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

        let (engine_thread, handle) = match self.open_store() {
            BddStore::Memory(store) => self.spawn_over(*store, directory),
            BddStore::Durable(store) => self.spawn_over(*store, directory),
        };

        self.engine = Some(engine_thread);
        self.handle = Some(handle);

        // Every signer an identity scenario registered, plus the ordinary
        // signer a logged-in `Given` implies. Both go through the same door
        // an app would use, AFTER the engine exists, exactly as a real launch
        // does -- the engine always starts with zero accounts.
        self.register_identity_signers();
        if let Some(active) = self.active_person.clone() {
            let keys = self.person(&active);
            if !self.identities_with_signers.contains(&active) {
                let signer = self.counting_signer(&keys);
                self.handle()
                    .add_signer(signer)
                    .expect("local signer has a public key");
            }
            self.handle().set_active_account(Some(keys.public_key()));
        }

        let (diag_handle, diag_rx) = self.handle().observe_diagnostics();
        self.diag = Some(DiagFeed::new(diag_handle, diag_rx));
    }

    /// Spawn over whichever store this scenario chose. Generic so the choice
    /// above is a value rather than a duplicated call, and so neither store
    /// type leaks into `NmpWorld` -- `EngineThread` is not generic, and the
    /// store is moved whole into the engine thread and never comes back.
    fn spawn_over<S>(&self, store: S, directory: LiveDirectory) -> (EngineThread, Handle)
    where
        S: EventStore + Send + 'static,
    {
        EngineThread::spawn(
            store,
            directory,
            20,
            PoolConfig {
                reconnect_delay_initial: Some(Duration::from_millis(20)),
                ..PoolConfig::default()
            },
            // The BDD harness injects its (local, in-process) relays straight
            // into the directory via `ingest_write_relays`, so they never pass
            // through the engine's discovered-relay admission gate (issue
            // #121). Opt those local hosts in anyway, so any scenario that DOES
            // exercise kind:10002 discovery of a scripted local relay is
            // admitted rather than silently dropped.
            nmp::mechanism::core::RelayAdmissionPolicy::new([
                "127.0.0.1".to_string(),
                "localhost".to_string(),
                "[::1]".to_string(),
                "::1".to_string(),
            ]),
        )
        .expect("BDD engine thread construction")
    }

    /// The store this scenario runs on.
    ///
    /// In memory by default, which is what the whole catalog has always used
    /// and what keeps its wall clock inside the crate's `timeout 240`
    /// contract: redb commits real transactions to real files, and a suite
    /// that ingests a fixture backlog per scenario pays that on every one of
    /// them.
    ///
    /// On disk when the scenario staged an identity by key. A `MemoryStore`
    /// cannot be reopened, so a world that will be asked to reconstruct its
    /// engine over the SAME store needs a real one -- and every restart step
    /// in the catalog belongs to `features/identity/`, whose scenarios all
    /// name their accounts that way (see `world::identity`). The flag is set
    /// by those `Given`s rather than inferred later because the store is
    /// chosen once, at start-up, before any `When` exists to ask.
    fn open_store(&mut self) -> BddStore {
        if !self.durable_store {
            return BddStore::Memory(Box::new(MemoryStore::new()));
        }
        let path = match &self.store_path {
            Some(path) => path.clone(),
            None => {
                let dir = tempfile::tempdir().expect("nmp-bdd: a temp dir for the durable store");
                let path = dir.path().join("bdd-store.redb");
                self.store_dir = Some(dir);
                self.store_path = Some(path.clone());
                path
            }
        };
        BddStore::Durable(Box::new(
            RedbStore::open(&path).expect("nmp-bdd: the scenario's durable store must open"),
        ))
    }
}

/// Which store a scenario got, so the two spawn arms below stay one decision
/// made in one place.
enum BddStore {
    /// Both variants are boxed. `MemoryStore` is the large one here (>=1024
    /// bytes), so leaving it inline would make every `BddStore` value carry
    /// that footprint; boxing both keeps the enum small whichever store grows
    /// next.
    Memory(Box<MemoryStore>),
    Durable(Box<RedbStore>),
}
