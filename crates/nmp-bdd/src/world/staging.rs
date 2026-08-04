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
//!
//! What this file stages is a PERSON'S protocol state and a RELAY'S behaviour.
//! What the engine has been able to LEARN about a person -- the three-valued
//! knowledge, and whether the discovery sources have finished looking -- is a
//! different axis and lives in [`super::outbox`] with the family that turns on
//! it. The two read alike and are not the same claim: staging a relay list is
//! saying what exists, while staging an unfinished lookup is saying what has
//! not been found yet.

use std::collections::BTreeSet;
use std::time::Duration;

use nostr::{Keys, PublicKey, Timestamp, UnsignedEvent};

use nmp::mechanism::runtime::Handle;
use nmp::Engine;
use nmp_router::FixtureRoutingFacts;
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

    /// This scenario will reconstruct its engine, so it needs a store that
    /// outlives one.
    ///
    /// Decided from the scenario's own steps rather than from a `Given`
    /// (`tests/bdd.rs`'s before-hook), because a `.feature` should not have
    /// to say which storage engine the harness picked -- it says "I
    /// reconstruct the engine from the same durable store", and that sentence
    /// IS the requirement. #974 chose the store at start-up and had to answer
    /// the question before any `When` existed to ask it; a hook that reads the
    /// whole scenario answers it from the same words the reader sees.
    pub fn use_durable_store(&mut self) {
        assert!(
            !self.started,
            "nmp-bdd: the store is chosen once, before anything runs"
        );
        self.durable_store = true;
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

    /// `Given relay <name> rejects every event` -- with the relay's OWN
    /// words when the scenario wrote them, because "blocked: not admitted"
    /// is actionable and a generic refusal is not.
    pub fn set_reject_writes(&mut self, relay: &str, message: Option<&str>) {
        self.relay_config_mut(relay).reject_writes = Some(
            message
                .unwrap_or("blocked: nmp-bdd scripted relay is configured to reject every event")
                .to_string(),
        );
    }

    pub fn set_reject_queries(&mut self, relay: &str) {
        self.relay_config_mut(relay).reject_queries = true;
    }

    /// Require a genuine NIP-42 challenge before this relay admits writes.
    pub fn require_write_auth(&mut self, relay: &str) {
        self.relay_config_mut(relay).auth_required_writes = true;
    }

    /// Stage the active account's app-owned denial for one named relay.
    ///
    /// The name cannot become a policy request URL until the relay binds, so
    /// registration happens in `spawn_engine` on every fresh construction.
    pub fn deny_write_auth_by_policy(&mut self, relay: &str, reason: &str) {
        self.relay_config_mut(relay);
        self.auth_policy_denials
            .insert(relay.to_string(), reason.to_string());
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

    /// `Given relay <name> advertises that NIP-77 is unsupported`.
    ///
    /// The scripted NIP-11 document names supported NIPs 1 and 11 only.
    /// Engine acquisition therefore records an explicit negative
    /// advertisement and does not start the behavioral NIP-77 probe. This is
    /// the deterministic relay shape for scenarios whose subject is the
    /// ordinary NIP-01 router plan: otherwise probe completion can race a
    /// demand mutation and legitimately overlap-close the prior REQ during a
    /// NIP-77 live-candidate handoff.
    pub fn advertise_nip77_unsupported(&mut self, relay: &str) {
        self.relay_config_mut(relay)
            .advertised_limits
            .get_or_insert_with(Default::default);
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
        // A contact list is an authored event just like a staged note. Give
        // it the same ordinary author-owned fixture route so the derived
        // query has a real source after the deleted generic indexer lane.
        // This must not put kind:3 back on operator NIP-65 sources.
        self.write_relay_for(person);
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
    ///
    /// One staging, and it always KEEPS the exact signed event. Routing and
    /// authorship are separate axes: republishing a note verbatim is the
    /// standing proof of that, and it only proves anything if the bytes that
    /// go back out are the bytes their author signed. There used to be a
    /// second, event-forgetting staging for the notes nobody republished; the
    /// only difference it made was that `features/writes/pre-signed-events`
    /// could not point at a note an ordinary `Given` had staged.
    pub fn stage_note(&mut self, person: &str, text: &str) {
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

    /// Bring a relay into a world that has ALREADY started.
    ///
    /// Ordinarily every relay is staged as a `Given` and started together,
    /// because a scenario knows its own topology up front. A relay first
    /// named by a relay list that ARRIVES mid-scenario cannot be: not knowing
    /// it yet is precisely the situation under test. Idempotent, and it
    /// leaves an already-running relay exactly as it is.
    pub(super) async fn start_relay_late(&mut self, name: &str) {
        if self.relays.contains_key(name) {
            return;
        }
        let config = self.relay_configs.get(name).cloned().unwrap_or_default();
        if !self.relay_configs.contains_key(name) {
            self.relay_order.push(name.to_string());
            self.relay_configs.insert(name.to_string(), config.clone());
        }
        let relay = ScriptedRelay::start(&config).await;
        self.relays.insert(name.to_string(), relay);
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
            let mut relay = ScriptedRelay::start(&config).await;
            // `Given relay "R" cannot connect`: bind it (so it has a real URL
            // a group can be constructed with and a real port nobody else can
            // take), then sever it, so a connection attempt is REFUSED rather
            // than quietly succeeding against a relay that answers nothing.
            if self.is_unreachable(&name) {
                relay.disconnect().await;
            }
            self.relays.insert(name, relay);
        }

        for pending in std::mem::take(&mut self.pending_contact_lists) {
            let author_keys = self.person(&pending.author);
            let follow_pks: Vec<PublicKey> = pending
                .follows
                .iter()
                .map(|name| self.person(name).public_key())
                .collect();
            let mut targets: Vec<String> = self.app_relay_names.clone();
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

        self.seed_staged_group_metadata().await;
        self.spawn_engine().await;
    }

    /// Build the neutral fact snapshot from the staged relay topology and spawn a REAL
    /// `EngineThread` over this scenario's durable store.
    ///
    /// Its own method, called by `ensure_started` and again by the identity
    /// plane's restart step, because "reconstruct the engine from the same
    /// durable store" only means anything if the second engine is built the
    /// same way the first was -- same routing facts, same relays, same
    /// admission policy, nothing carried over in memory.
    pub(super) async fn spawn_engine(&mut self) {
        // Static neutral facts are a BDD-only snapshot. Indexers remain
        // staged protocol sources; generic routing does not see them.
        let app_urls = self.app_relay_urls();
        let fallback_urls = self.fallback_relay_urls();
        let mut facts = FixtureRoutingFacts::new()
            .with_operator_app(app_urls)
            .with_operator_fallback(fallback_urls);
        let mut authors = BTreeSet::new();
        authors.extend(self.write_relay_of.keys().cloned());
        authors.extend(self.read_relay_of.keys().cloned());
        authors.extend(self.declares_no_relays.iter().cloned());
        authors.extend(self.declares_no_write_relays.iter().cloned());
        for person in authors {
            let outbound = self
                .write_relay_of
                .get(&person)
                .into_iter()
                .flatten()
                .map(|name| self.relays[name].url.clone())
                .collect::<Vec<_>>();
            let inbound = self
                .read_relay_of
                .get(&person)
                .into_iter()
                .flatten()
                .map(|name| self.relays[name].url.clone())
                .collect::<Vec<_>>();
            facts = facts.with_author_routes(self.person(&person).public_key(), outbound, inbound);
        }

        // Which store, decided where the decision is: in memory by default,
        // which is what the whole catalog has always used and what keeps its
        // wall clock inside the crate's `timeout 240` contract -- redb
        // commits real transactions to real files, and a suite that ingests a
        // fixture backlog per scenario would pay that on every one of them.
        //
        // On disk when the scenario staged an identity by key. A
        // `MemoryStore` cannot be reopened, so a world that will be asked to
        // reconstruct its engine over the SAME store needs a real one -- and
        // every restart step in the catalog belongs to `features/identity/`,
        // whose scenarios all name their accounts that way (see
        // `world::identity`). The flag is set by those `Given`s rather than
        // inferred later because the store is chosen once, at start-up,
        // before any `When` exists to ask.
        let (engine, handle) = if self.durable_store {
            let store = self.open_durable_store();
            self.spawn_over(store, facts)
        } else {
            self.spawn_over(MemoryStore::new(), facts)
        };

        self.engine = Some(engine);
        self.handle = Some(handle);

        // Before any signer is registered and before any write exists: a
        // scenario that stated what time it is means the engine ran at that
        // time from its first tick, and a restart builds a brand-new engine
        // whose clock starts unpinned again.
        self.apply_clock();

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

            self.auth_policy_registrations.clear();
            if !self.auth_policy_denials.is_empty() {
                let denied = self
                    .auth_policy_denials
                    .iter()
                    .map(|(relay, reason)| {
                        (
                            self.relays
                                .get(relay)
                                .unwrap_or_else(|| {
                                    panic!("nmp-bdd: unknown AUTH-policy relay {relay:?}")
                                })
                                .url
                                .clone(),
                            reason.clone(),
                        )
                    })
                    .collect();
                let registration = self
                    .engine
                    .as_ref()
                    .expect("nmp-bdd: engine exists before policy registration")
                    .add_auth_policy(keys.public_key(), super::auth::StagedAuthPolicy { denied })
                    .expect("nmp-bdd: staged AUTH policy must register");
                self.auth_policy_registrations.push(registration);
            }
        }

        let (diag_handle, diag_rx) = self.handle().observe_diagnostics();
        self.diag = Some(DiagFeed::new(diag_handle, diag_rx));
    }

    /// Spawn over whichever store this scenario chose. Generic so the choice
    /// above is a value rather than a duplicated call, and so neither store
    /// type leaks into `NmpWorld` -- `Engine` is not generic, and the store is
    /// moved whole into the engine thread and never comes back.
    ///
    /// ONE engine, TWO surfaces. The product verbs a scenario is about run
    /// through `Engine` (`features/groups/` publishes through the
    /// `GroupOperations` extension trait on exactly this value); the raw
    /// delta and diagnostics channels a `Then` step has to FOLD run through
    /// the same engine's `Handle`. See `Engine::mechanism_handle`'s doc for
    /// why a fixture that owns both ends may hold both.
    fn spawn_over<S>(&self, store: S, facts: FixtureRoutingFacts) -> (Engine, Handle)
    where
        S: EventStore + Send + 'static,
    {
        let nip65_sources = self
            .indexer_names
            .iter()
            .map(|name| self.relays[name].url.clone())
            .collect();
        let engine = Engine::from_parts_with_fixture_routing_facts_and_nip65_sources(
            store,
            facts,
            nip65_sources,
            20,
            PoolConfig {
                reconnect_delay_initial: Some(Duration::from_millis(20)),
                ..PoolConfig::default()
            },
            // Static fixture facts do not pass through the network-discovery
            // gate. Opt local hosts in because feature-on NIP-65 scenarios do
            // discover routes from scripted local relay-list events.
            nmp::mechanism::core::RelayAdmissionPolicy::new(
                ["127.0.0.1", "localhost", "[::1]", "::1"].map(str::to_string),
                nmp::mechanism::core::OnionReachability::Unreachable,
            ),
        )
        .expect("BDD engine construction");
        let handle = engine
            .mechanism_handle()
            .expect("a freshly built engine is open");
        (engine, handle)
    }

    /// Stop the engine and give up every handle onto it, blocking until its
    /// threads have actually exited.
    ///
    /// One method because two callers need exactly this and must not drift:
    /// the identity plane's restart (`world::identity::restart_engine`, which
    /// then reopens the SAME durable store) and any teardown that wants the
    /// process boundary to be real rather than a handle swap. `Engine`'s own
    /// `shutdown` is what asks the engine thread to stop and joins it, and it
    /// is idempotent -- but the cloned `Handle` above has to go too, or the
    /// world keeps a way to talk to a stopped engine.
    pub(super) fn stop_engine(&mut self) {
        // The cloned handle goes FIRST, before anything blocks: `shutdown`
        // joins the engine thread, and a fixture that were still holding a way
        // to talk to it across that join would be describing a process that
        // has not actually stopped.
        self.handle = None;
        if let Some(engine) = self.engine.take() {
            engine.shutdown();
        }
    }

    /// This scenario's on-disk store, at a path created on FIRST use and kept
    /// for the whole scenario.
    ///
    /// The path outliving the engine is the entire point: reconstructing the
    /// engine opens this same file again, so what the second engine reads is
    /// the journal the first one wrote, and nothing else.
    fn open_durable_store(&mut self) -> RedbStore {
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
        RedbStore::open(&path).expect("nmp-bdd: the scenario's durable store must open")
    }
}

#[cfg(test)]
mod tests {
    use nmp_test_support::relays::AdvertisedLimits;

    use super::NmpWorld;

    #[test]
    fn nip77_unsupported_is_an_explicit_document_not_missing_information() {
        let mut world = NmpWorld::default();

        world.advertise_nip77_unsupported("hub");

        assert_eq!(
            world.relay_configs["hub"].advertised_limits,
            Some(AdvertisedLimits::default()),
            "unsupported must publish the explicit NIP-11 shape whose supported_nips excludes 77"
        );
    }
}
