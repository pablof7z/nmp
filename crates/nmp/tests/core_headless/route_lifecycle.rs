//! Routing as a strategy re-executed at every send opportunity (#975).
//!
//! These are the properties a scenario cannot reach through the wire tier
//! without a second process or a clock: retirement on SETTLED knowledge
//! rather than delivery, diff-and-append across repeated executions, and the
//! park that replaced a terminal failure. Everything here drives a real
//! `EngineCore` against a real `LiveDirectory`; nothing stubs resolution.

use super::*;

use std::cell::RefCell;

use nmp_router::{Lane, LanedRelay, LiveDirectory, PubkeyHex, RelayDirectory, RelayListKnowledge};

fn write_lane(url: &RelayUrl) -> Vec<LanedRelay> {
    vec![LanedRelay::new(url.clone(), Lane::Nip65Write)]
}

fn read_lane(url: &RelayUrl) -> Vec<LanedRelay> {
    vec![LanedRelay::new(url.clone(), Lane::Nip65Read)]
}

/// The engine's own live directory, shared with the test.
///
/// The whole subject here is a directory that LEARNS between one resolution
/// moment and the next, so a test has to be able to teach it after the engine
/// already holds it — which the ordinary construction path deliberately does
/// not allow (the engine owns its directory, and no app writes to it).
/// Sharing one cell is the smallest way to say "the world learned something"
/// without inventing an engine door that only tests would use.
#[derive(Clone)]
struct SharedDirectory(Rc<RefCell<LiveDirectory>>);

impl SharedDirectory {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(
            LiveDirectory::builder()
                .indexers([RelayUrl::parse("wss://indexer.example").unwrap()])
                .build(),
        )))
    }

    fn learns_write(&self, author: &Keys, relay: &RelayUrl) {
        self.0
            .borrow_mut()
            .ingest_write_relays(author.public_key().to_hex(), write_lane(relay));
    }

    fn learns_read(&self, author: &Keys, relay: &RelayUrl) {
        self.0
            .borrow_mut()
            .ingest_read_relays(author.public_key().to_hex(), read_lane(relay));
    }

    /// Discovery finished having sent nothing for this author — the ONE
    /// transition that mints `KnownAbsent`.
    fn settles_absent(&self, author: &Keys) {
        self.0
            .borrow_mut()
            .settle_relay_list_absent(author.public_key().to_hex());
    }
}

impl RelayDirectory for SharedDirectory {
    fn write_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay> {
        self.0.borrow().write_relays(author)
    }

    fn extra_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay> {
        self.0.borrow().extra_relays(author)
    }

    fn indexers(&self) -> Vec<RelayUrl> {
        self.0.borrow().indexers()
    }

    fn pinned_relays(&self, atom: &ConcreteFilter) -> Vec<LanedRelay> {
        self.0.borrow().pinned_relays(atom)
    }

    fn app_relays(&self) -> Vec<RelayUrl> {
        self.0.borrow().app_relays()
    }

    fn fallback_relays(&self) -> Vec<RelayUrl> {
        self.0.borrow().fallback_relays()
    }

    fn read_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay> {
        self.0.borrow().read_relays(author)
    }

    fn relay_list_knowledge(&self, author: &PubkeyHex) -> RelayListKnowledge {
        self.0.borrow().relay_list_knowledge(author)
    }

    fn settle_relay_list_absent(&mut self, author: PubkeyHex) {
        self.0.borrow_mut().settle_relay_list_absent(author);
    }

    fn ingest_write_relays(&mut self, author: PubkeyHex, relays: Vec<LanedRelay>) {
        self.0.borrow_mut().ingest_write_relays(author, relays);
    }

    fn ingest_read_relays(&mut self, author: PubkeyHex, relays: Vec<LanedRelay>) {
        self.0.borrow_mut().ingest_read_relays(author, relays);
    }
}

/// Publish `builder` as `author` and drive it through its signer, returning
/// the receipt and every effect the signature produced.
fn publish_and_sign<S: EventStore>(
    core: &mut EngineCore<S>,
    author: &Keys,
    builder: nmp_grammar::EventBuilder,
) -> (ReceiptId, Vec<Effect>) {
    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(builder),
        durability: Durability::Durable,
        routing: WriteRouting::Auto,
        identity_override: None,
        correlation: None,
    }));
    let (id, generation, unsigned) = find_sign_request(&accepted);
    let signed = unsigned.sign_with_keys(author).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    (id, effects)
}

fn statuses(effects: &[Effect], id: ReceiptId) -> Vec<WriteStatus> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitReceipt(receipt, status) if *receipt == id => Some(status.clone()),
            _ => None,
        })
        .collect()
}

fn routed(effects: &[Effect], id: ReceiptId) -> Option<(BTreeSet<RelayUrl>, bool)> {
    statuses(effects, id)
        .into_iter()
        .rev()
        .find_map(|status| match status {
            WriteStatus::Routed { relays, complete } => Some((relays, complete)),
            _ => None,
        })
}

/// The defect this design removes, at the reducer door: publishing before the
/// author's first relay-list fetch used to remove the pending write and emit
/// a terminal `Failed`. It parks instead — accepted, signed, durable, and
/// naming what it waits for.
#[test]
fn a_write_with_nothing_to_route_to_parks_and_is_never_failed() {
    let author = Keys::generate();
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(SharedDirectory::new()), 10);
    activate(&mut core, &author);

    let (id, effects) = publish_and_sign(&mut core, &author, draft(1, "cold start"));
    let seen = statuses(&effects, id);

    assert!(
        !seen.iter().any(|s| matches!(s, WriteStatus::Failed(_))),
        "the event is signed, journalled and durable and the app did everything right; \
         only the directory was young: {seen:?}"
    );
    let parked = seen
        .iter()
        .find_map(|s| match s {
            WriteStatus::AwaitingRoute { detail } => Some(detail.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a routing park, saw {seen:?}"));
    assert!(
        parked.contains(&author.public_key().to_hex()),
        "a park with no reason is barely better than losing the write: {parked}"
    );
    let replay = core.reattach_receipt(id);
    assert!(
        replay.facts.iter().any(
            |status| matches!(status, WriteStatus::AwaitingRoute { detail } if detail == &parked)
        ),
        "the park is retained and replayed VERBATIM, so a route parked for a month is \
         still visible with the same reason: {:?}",
        replay.facts
    );
}

/// Moment three, and the property that makes every other moment free: the
/// strategy runs again on the ordinary tick against whatever the directory has
/// learned since — with no wiring at all between whatever taught it and the
/// write plane.
#[test]
fn a_parked_write_routes_itself_once_the_directory_learns() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://outbox-a.example").unwrap();
    let directory = SharedDirectory::new();
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(directory.clone()), 10);
    activate(&mut core, &author);

    let (id, first) = publish_and_sign(&mut core, &author, draft(2, "waits"));
    assert!(routed(&first, id).is_none(), "nothing was routable yet");

    // The directory learns the author's relay list for some completely
    // unrelated reason -- a profile opened, a feed hydrated.
    directory.learns_write(&author, &relay);

    let ticked = core.handle(EngineMsg::Tick(Timestamp::from(1_000)));
    let (relays, complete) = routed(&ticked, id)
        .unwrap_or_else(|| panic!("expected a route, saw {:?}", statuses(&ticked, id)));
    assert_eq!(relays, BTreeSet::from([relay.clone()]));
    assert!(
        complete,
        "one Known author and no unknowns left is a retired Auto"
    );
    assert!(
        ticked.iter().any(|effect| matches!(
            effect,
            Effect::EnsureWriteRelay(session)
                if session == &signer_session(&relay, author.public_key())
        )),
        "the newly revealed relay must mint a real delivery obligation"
    );
}

/// Diff-and-append, observed as the absence of work: re-executing a strategy
/// that learned nothing costs an empty diff, so the receipt says nothing new
/// and no second lane is minted for a relay the intent already has.
#[test]
fn re_executing_an_unchanged_strategy_mints_nothing() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://outbox-a.example").unwrap();
    let directory = SharedDirectory::new();
    directory.learns_write(&author, &relay);
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(directory), 10);
    activate(&mut core, &author);

    let (id, first) = publish_and_sign(&mut core, &author, draft(3, "steady"));
    assert_eq!(
        routed(&first, id).map(|(_, complete)| complete),
        Some(true),
        "a Known author with no p-tags has nothing left to learn"
    );

    for tick in 0..5u64 {
        let later = core.handle(EngineMsg::Tick(Timestamp::from(2_000 + tick)));
        assert!(
            statuses(&later, id).is_empty(),
            "a retired route is never re-executed, so tick {tick} must be silent on this \
             receipt: {:?}",
            statuses(&later, id)
        );
    }
}

/// Retirement is knowledge exhaustion, and this is the case that makes the
/// distinction load-bearing: a p-tagged recipient whose relay list is SETTLED
/// ABSENT contributes nothing and blocks nothing, while one that is merely
/// `Unknown` keeps the whole obligation alive. Same empty relay set, opposite
/// verdicts.
#[test]
fn a_settled_absence_retires_an_auto_that_an_unknown_would_not() {
    let author = Keys::generate();
    let recipient = Keys::generate();
    let relay = RelayUrl::parse("wss://outbox-a.example").unwrap();

    let directory = SharedDirectory::new();
    directory.learns_write(&author, &relay);
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(directory.clone()), 10);
    activate(&mut core, &author);

    let mentions = draft(4, "hello you").tag(nostr::Tag::public_key(recipient.public_key()));
    let (id, first) = publish_and_sign(&mut core, &author, mentions);
    let (relays, complete) = routed(&first, id).expect("the author's own relay is routable now");
    assert_eq!(relays, BTreeSet::from([relay.clone()]));
    assert!(
        !complete,
        "one recipient is still Unknown, so the answer can still change"
    );

    // Discovery finishes having sent nothing for the recipient. That is a
    // POSITIVE fact, not a timeout.
    directory.settles_absent(&recipient);

    let ticked = core.handle(EngineMsg::Tick(Timestamp::from(3_000)));
    assert_eq!(
        routed(&ticked, id),
        Some((BTreeSet::from([relay]), true)),
        "zero unknowns remain, so the answer can never change again and the Auto retires \
         with exactly the resolvable relay set: {:?}",
        statuses(&ticked, id)
    );
}

/// The one an app is most likely to get wrong: a recipient WITH a relay list
/// contributes their inbox — their READ relays, never their write set.
#[test]
fn a_recipient_is_reached_at_their_inbox_and_never_at_their_write_relays() {
    let author = Keys::generate();
    let recipient = Keys::generate();
    let mine = RelayUrl::parse("wss://outbox-a.example").unwrap();
    let their_inbox = RelayUrl::parse("wss://their-inbox.example").unwrap();
    let their_outbox = RelayUrl::parse("wss://their-outbox.example").unwrap();

    let directory = SharedDirectory::new();
    directory.learns_write(&author, &mine);
    directory.learns_read(&recipient, &their_inbox);
    directory.learns_write(&recipient, &their_outbox);
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(directory), 10);
    activate(&mut core, &author);

    let mentions = draft(5, "hello you").tag(nostr::Tag::public_key(recipient.public_key()));
    let (id, effects) = publish_and_sign(&mut core, &author, mentions);
    let (relays, complete) = routed(&effects, id).expect("both inputs are Known");

    assert_eq!(
        relays,
        BTreeSet::from([mine, their_inbox]),
        "the fan-out is the author's own outbox plus the recipient's INBOX"
    );
    assert!(
        !relays.contains(&their_outbox),
        "a recipient's write relays are where they PUBLISH, never where they are reached"
    );
    assert!(complete, "two Known inputs leave nothing to learn");
}

/// A resolution that named NOTHING has not decided anything, so it cannot
/// retire — even when every input it has is settled. Calling that complete
/// would strand the write permanently, because a retired route is never
/// re-executed, which is the very failure mode this design exists to remove.
#[test]
fn a_route_that_names_nothing_never_retires() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://outbox-a.example").unwrap();
    let directory = SharedDirectory::new();
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(directory.clone()), 10);
    activate(&mut core, &author);

    let (id, first) = publish_and_sign(&mut core, &author, draft(7, "nowhere yet"));
    assert!(routed(&first, id).is_none());

    // Even fully settled, an author with no relay list of their own and no
    // app relay configured leaves this write with no destination at all.
    directory.settles_absent(&author);
    let ticked = core.handle(EngineMsg::Tick(Timestamp::from(4_000)));
    assert!(
        !statuses(&ticked, id)
            .iter()
            .any(|status| matches!(status, WriteStatus::Routed { complete: true, .. })),
        "zero destinations is an unroutable park, never a decided route: {:?}",
        statuses(&ticked, id)
    );

    // And because it never retired, a relay list arriving afterwards still
    // reaches it.
    directory.learns_write(&author, &relay);
    let later = core.handle(EngineMsg::Tick(Timestamp::from(5_000)));
    assert_eq!(
        routed(&later, id),
        Some((BTreeSet::from([relay]), true)),
        "the write was held, not abandoned, so it completes on its own: {:?}",
        statuses(&later, id)
    );
}
