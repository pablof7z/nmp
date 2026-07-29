//! `When`-time acts: the app opens a feed or publishes, the person at the
//! keyboard switches account, ANOTHER user posts, or the network takes a
//! relay away and gives it back.
//!
//! What unites these is that each one is a STIMULUS -- it changes the world
//! and returns, leaving the assertion to a later `Then` -- and that each is
//! phrased as something a person or the network does, never as a call. They
//! sit apart from [`super::staging`] because they run against an already
//! started engine (each begins by `ensure_started`ing it) and apart from
//! [`super::watches`] because they drive the FEED, the app-visible channel,
//! rather than the direct-to-one-relay socket observations.

use nostr::{PublicKey, Tag};

use nmp_grammar::{Durability, EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};

use nmp_test_support::relays::ScriptedRelay;

use super::budgets::RECONNECT;
use super::observe::{FeedState, ReceiptState};
use super::queries::my_follows_query;
use super::NmpWorld;

impl NmpWorld {
    /// `When I open a feed of my follows' notes` / the `Given` shorthand
    /// `my feed of my follows' notes is open`.
    pub async fn open_my_follows_feed(&mut self) {
        self.ensure_started().await;
        let (handle_id, rx) = self
            .handle()
            .subscribe(my_follows_query())
            .expect("BDD subscription construction");
        self.feed = Some(FeedState::new(handle_id, rx));
    }

    /// `When I open a feed of the latest <n> of my follows' notes` -- the
    /// same feed as [`Self::open_my_follows_feed`], bounded.
    ///
    /// The bound goes on the OUTER selection, which is what makes this a
    /// per-feed window: one `limit` for the whole feed, carried by a demand
    /// whose author binding still resolves to the full follow list. The
    /// resolver then fans that demand into one atom per author for routing,
    /// every atom carrying the same `limit` -- and it is the re-join of those
    /// atoms into one REQ per relay that `bounded-feed-window.feature`
    /// exercises (#937).
    pub async fn open_my_follows_feed_limited(&mut self, limit: usize) {
        self.ensure_started().await;
        let mut query = my_follows_query();
        query.0.selection.limit = Some(limit);
        let (handle_id, rx) = self
            .handle()
            .subscribe(query)
            .expect("BDD subscription construction");
        self.feed = Some(FeedState::new(handle_id, rx));
    }

    /// `When I publish a new follow list with <people>`.
    pub async fn publish_new_follow_list(&mut self, people: &[String]) {
        self.ensure_started().await;
        // The BEFORE half of "untouched since here" is itself a read of a
        // moving wire, and it is the half that was racing (#949). Opening a
        // feed leaves traffic in flight -- notably `apply_replay`'s
        // documented duplicate REQ at connect ("never more than two sends of
        // one filter", `docs/internals/subscriptions/
        // identity-grouping-and-limits.md` §5.4) -- and a contact count
        // sampled mid-flight is one short. The `Then` then attributes that
        // still-arriving startup frame to THIS publish and fails, which is
        // why the flake was load-sensitive: only a slow enough machine let
        // the duplicate cross the snapshot boundary. Settling here makes the
        // baseline the steady state it claims to be. Settling in the `Then`
        // instead cannot help: the counter only grows, so reading it later
        // can only add touches, never remove them.
        self.wire_settled().await;
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
        let _ = me_keys;
        let rx = self
            .handle()
            .publish(WriteIntent {
                payload: WritePayload::Event(EventBuilder {
                    kind: nostr::Kind::ContactList,
                    tags,
                    content: String::new(),
                    created_at: None,
                }),
                durability: Durability::Durable,
                routing: WriteRouting::Auto,
                identity: Identity::Active,
                correlation: None,
            })
            .expect("BDD receipt correlation namespace must be available");
        self.receipts.push(ReceiptState::new(rx));
    }

    /// `When I publish a note saying <text>`.
    pub async fn publish_note(&mut self, text: &str) {
        self.ensure_started().await;
        let me = self
            .active_person
            .clone()
            .expect("nmp-bdd: publishing a note needs a logged-in account");
        let _ = self.person(&me);
        self.publish_intent(WriteIntent {
            payload: WritePayload::Event(EventBuilder::new(nostr::Kind::TextNote).content(text)),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        });
    }

    /// `When I publish kind <kind> with d tag <d> saying <text>`.
    ///
    /// The explicit monotonically-increasing timestamp makes the second
    /// publication the NIP-01 winner without relying on wall-clock
    /// granularity. Only addressable kinds carry the `d` tag; ordinary
    /// replaceable kinds are keyed by `(pubkey, kind)` regardless of tags.
    pub async fn publish_replaceable(&mut self, kind: u16, d: &str, text: &str) {
        self.ensure_started().await;
        let me = self
            .active_person
            .clone()
            .expect("nmp-bdd: publishing a replaceable event needs a logged-in account");
        let _ = self.person(&me);
        let created_at = self.next_created_at();
        let mut builder = EventBuilder::new(nostr::Kind::from(kind))
            .content(text)
            .created_at(nostr::Timestamp::from(created_at));
        if (30_000..=39_999).contains(&kind) {
            builder = builder.tag(Tag::identifier(d));
        }
        self.publish_intent(WriteIntent {
            payload: WritePayload::Event(builder),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        });
    }

    /// `When I publish a note saying "..." mentioning <people>` -- an
    /// ordinary `Auto` note that p-tags one or more recipients, which is what
    /// makes the outbox fan-out (and therefore its unknowns) reachable from a
    /// scenario at all.
    pub async fn publish_note_mentioning(&mut self, text: &str, people: &[String]) {
        self.ensure_started().await;
        let me = self
            .active_person
            .clone()
            .expect("nmp-bdd: publishing a note needs a logged-in account");
        let _ = self.person(&me);
        let tags: Vec<Tag> = people
            .iter()
            .map(|name| Tag::public_key(self.person(name).public_key()))
            .collect();
        let mut builder = EventBuilder::new(nostr::Kind::TextNote).content(text);
        builder.tags = tags;
        self.publish_intent(WriteIntent {
            payload: WritePayload::Event(builder),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        });
    }

    /// `When <person>'s relay list arrives naming <relay> as their read/write
    /// relay` -- a real kind:10002 landing at the indexers, exactly where the
    /// engine's own discovery subscription is looking.
    ///
    /// Nothing is injected into the directory here: the event goes on a
    /// relay and comes back through the engine's ordinary ingestion, which is
    /// the only way this proves that a parked route wakes on knowledge the
    /// READ path acquired.
    pub async fn relay_list_arrives(&mut self, person: &str, write: &[String], read: &[String]) {
        self.ensure_started().await;
        // A relay list learned at RUNTIME names relays no `Given` could have
        // staged -- that is the whole situation ("my relay list has never
        // been fetched"). Start any such relay now, on the same footing as
        // every other: the engine reaches it only if routing sends it there.
        for name in write.iter().chain(read) {
            self.start_relay_late(name).await;
        }
        let keys = self.person(person);
        let write_urls: Vec<String> = write
            .iter()
            .map(|name| self.relay_url(name).to_string())
            .collect();
        let read_urls: Vec<String> = read
            .iter()
            .map(|name| self.relay_url(name).to_string())
            .collect();
        let created_at = self.next_created_at();
        for indexer in self.indexer_names.clone() {
            self.relays[&indexer]
                .seed_relay_list(&keys, &write_urls, &read_urls, created_at)
                .await;
        }
    }

    /// `When I publish a note saying "..." to exactly <relays>` -- the app
    /// naming its own destinations. `relays` may name relays no `Given`
    /// mentioned (an app publishing to a relay a user typed into a text
    /// field is the point), so any unknown name is registered as an
    /// ordinary well-behaved relay before the engine starts.
    ///
    /// An EMPTY list is passed through unchanged: refusing it is the
    /// engine's job, at its acceptance door, and a world that quietly
    /// declined to make the call could not observe that.
    pub async fn publish_note_to_exactly(&mut self, text: &str, relay_names: &[String]) {
        for name in relay_names {
            if !self.relay_configs.contains_key(name) {
                self.register_bystander_relay(name);
            }
        }
        self.ensure_started().await;
        let me = self
            .active_person
            .clone()
            .expect("nmp-bdd: publishing a note needs a logged-in account");
        let _ = self.person(&me);
        let routing = self.explicit_routing(relay_names);
        self.snapshot_relay_contacts();
        self.publish_intent(WriteIntent {
            payload: WritePayload::Event(EventBuilder::new(nostr::Kind::TextNote).content(text)),
            durability: Durability::Durable,
            routing,
            identity: Identity::Active,
            correlation: None,
        });
    }

    /// `When I publish <person>'s signed note unchanged to exactly <relay>`
    /// -- the archive-republish case. The payload is the event that person
    /// signed, byte for byte; the route is mine. Nothing about either was
    /// consumed by the other.
    pub async fn republish_signed_note_to_exactly(&mut self, text: &str, relay_names: &[String]) {
        for name in relay_names {
            if !self.relay_configs.contains_key(name) {
                self.register_bystander_relay(name);
            }
        }
        self.ensure_started().await;
        let event = self
            .signed_notes
            .get(text)
            .cloned()
            .expect("nmp-bdd: republishing needs a note staged as already-signed");
        let routing = self.explicit_routing(relay_names);
        self.snapshot_relay_contacts();
        self.republished = Some(event.clone());
        self.publish_intent(WriteIntent {
            payload: WritePayload::Signed(event),
            durability: Durability::Durable,
            routing,
            identity: Identity::Active,
            correlation: None,
        });
    }

    fn explicit_routing(&self, relay_names: &[String]) -> WriteRouting {
        WriteRouting::Explicit(
            relay_names
                .iter()
                .map(|name| self.relay_url(name))
                .collect(),
        )
    }

    /// Every publish in this module goes through here: it records whether the
    /// app named relays (for `last_publish_named_no_relay`) and opens the one
    /// receipt stream the publish owns.
    fn publish_intent(&mut self, intent: WriteIntent) {
        self.last_publish_was_auto = matches!(intent.routing, WriteRouting::Auto);
        let rx = self
            .handle()
            .publish(intent)
            .expect("BDD receipt correlation namespace must be available");
        self.receipts.push(ReceiptState::new(rx));
    }

    /// `When I switch to <person>'s account` (a person already known to the
    /// world, e.g. previously logged in as).
    pub async fn switch_account(&mut self, person: &str) {
        self.ensure_started().await;
        let keys = self.person(person);
        self.handle()
            .add_signer(self.counting_signer(&keys))
            .expect("BDD local signer always exposes its public key");
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
        self.handle()
            .add_signer(self.counting_signer(&keys))
            .expect("BDD local signer always exposes its public key");
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
    pub async fn drop_relay_connection(&mut self, name: &str) {
        // A scenario may take the network away BEFORE it ever publishes, so
        // this is the step that starts the engine. Dropping a relay the
        // engine has not connected to yet would otherwise leave the first
        // publish to connect to a live relay.
        self.ensure_started().await;
        self.relays
            .get_mut(name)
            .unwrap_or_else(|| panic!("nmp-bdd: relay {name:?} must exist before it can drop"))
            .disconnect()
            .await;
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
        // `drop_relay_connection` awaits `ConnectionOwner::shutdown`, so the
        // public listener and established client stream are already gone and
        // this exact address is immediately safe to rebind.
        let fresh = ScriptedRelay::start_on_port(port, &config).await;
        assert!(
            fresh.wait_contacted(RECONNECT).await,
            "nmp-bdd: relay {name:?} was not recontacted within {RECONNECT:?} of coming back -- \
             the engine's Pool did not reconnect/resubscribe in time"
        );
        self.relays.insert(name.to_string(), fresh);
    }
}
