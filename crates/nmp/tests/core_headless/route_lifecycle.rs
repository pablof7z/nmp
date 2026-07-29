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

/// One reconcile round played by a relay that holds NOTHING for the filter --
/// a real `negentropy` peer over an empty sealed storage, so the reply is
/// whatever the protocol actually produces rather than a hand-written payload
/// that happens to decode. This is the wire fact "I have no such events".
fn empty_relay_reconcile_reply(initial_hex: &str) -> String {
    let mut storage = ::negentropy::NegentropyStorageVector::new();
    storage
        .seal()
        .expect("an empty storage always seals cleanly");
    let mut relay_side = ::negentropy::Negentropy::owned(storage, 0)
        .expect("frame_size_limit=0 (unlimited) is always valid");
    let raw = hex::decode(initial_hex).expect("the engine's own hex must round-trip");
    let reply = relay_side
        .reconcile(&raw)
        .expect("a well-formed initiator message must reconcile");
    hex::encode(reply)
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
        identity: Identity::Active,
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

/// #1019's falsifier, first half: **a relay-list question survives the request
/// that carried it being taken off the wire, and is answered by whatever
/// terminal signal that request actually gets.**
///
/// Against a NIP-77 relay the planned discovery REQ is never sent as an
/// ordinary REQ at all. It is replaced by a `limit:0` barrier — which attests
/// NOTHING, the relay sends no events by construction — and the real question
/// is re-asked inside a negentropy session under a role-derived id that is in
/// no plan and never EOSEs. Settlement used to be looked up in the router
/// plan by the EOSE's subscription id, so on every NIP-77-capable relay it
/// could not fire at all: the indexers finished, the answer arrived, and the
/// write parked on that recipient's relay list stayed parked forever. Under
/// durable parking that does not lose the write; it strands it, which is
/// harder to diagnose and no better to be on the receiving end of.
///
/// This is the deterministic instance of the class the issue names. It is not
/// an unlucky interleaving: it is what a NIP-77 relay does every time.
#[test]
fn reconciliation_answers_the_relay_list_question_the_planned_req_never_asked() {
    let author = Keys::generate();
    let recipient = Keys::generate();
    let other = Keys::generate();
    let outbox = RelayUrl::parse("wss://outbox-a.example").unwrap();
    // `SharedDirectory`'s single configured indexer, which is what a
    // discovery-kind atom is routed to and therefore what has to finish
    // before an absence may settle.
    let indexer = RelayUrl::parse("wss://indexer.example").unwrap();

    let directory = SharedDirectory::new();
    directory.learns_write(&author, &outbox);
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(directory), 10);
    activate(&mut core, &author);

    // A p-tagged recipient whose relay list is unknown parks the write and
    // opens discovery on the indexer -- which is what puts that relay in the
    // plan, and therefore what makes probing it legitimate at all.
    let mentions = draft(20, "hello you").tag(nostr::Tag::public_key(recipient.public_key()));
    let (parked, opened) = publish_and_sign(&mut core, &author, mentions);
    assert_eq!(
        routed(&opened, parked).map(|(_, complete)| complete),
        Some(false),
        "an Unknown recipient keeps the obligation alive: {:?}",
        statuses(&opened, parked)
    );

    // Prove NIP-77 support behaviorally, exactly as a live relay does: the
    // engine probes on connection and any valid NEG-MSG reply classifies it.
    let connected = connect(&mut core, 0, &indexer);
    let probe_sub = connected
        .iter()
        .find_map(|effect| match effect {
            Effect::StartProbe(url, sub_id, ..) if url == &indexer => Some(sub_id.clone()),
            _ => None,
        })
        .expect("connecting a planned, never-probed relay must start a capability probe");
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&indexer),
        neg_msg_frame(&wire_sub_string(&probe_sub), "6100"),
    ));

    // A second unknown recipient widens the discovery filter, which is what
    // puts that question back on the wire. The relay is Supported now and the
    // filter is broad, so what reaches the wire is the handoff's `limit:0`
    // barrier -- never the planned REQ.
    let second = draft(21, "hello you too").tag(nostr::Tag::public_key(other.public_key()));
    let (_, rerouted) = publish_and_sign(&mut core, &author, second);
    let (live_sub, live_filter) = req_for_kind(&rerouted, &indexer, 10002);
    let live_sub = live_sub.clone();
    assert_eq!(
        live_filter.limit,
        Some(0),
        "the discovery question rides a barrier that attests nothing, which is exactly why \
         its EOSE must not settle anything"
    );

    // The barrier's EOSE opens reconciliation. It settles NOTHING on its own.
    let opened_neg = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&indexer),
        eose_frame(&wire_sub_string(&live_sub)),
    ));
    assert!(
        routed(&opened_neg, parked).is_none_or(|(_, complete)| !complete),
        "a limit:0 EOSE proves nothing about whether a kind:10002 exists: {:?}",
        statuses(&opened_neg, parked)
    );
    let (neg_sub, initial_hex) = opened_neg
        .iter()
        .find_map(|effect| match effect {
            Effect::NegOpen(_, sub_id, _, hex) => Some((sub_id.clone(), hex.clone())),
            _ => None,
        })
        .expect("the barrier EOSE must open Negentropy");

    // Reconcile against a relay that genuinely holds no kind:10002 for this
    // recipient -- a real negentropy peer over an empty sealed storage, not a
    // hand-written payload.
    let reply = empty_relay_reconcile_reply(&initial_hex);
    let settled = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&indexer),
        neg_msg_frame(&wire_sub_string(&neg_sub), &reply),
    ));

    assert_eq!(
        routed(&settled, parked),
        Some((BTreeSet::from([outbox]), true)),
        "reconciliation finishing with nothing to fetch IS the answer that this indexer has \
         no relay list for the recipient, so the write retires in the same turn rather than \
         parking forever: {:?}",
        statuses(&settled, parked)
    );
    assert!(
        !statuses(&settled, parked)
            .iter()
            .any(|status| matches!(status, WriteStatus::Failed(_))),
        "settling absence is a positive fact, never a failure: {:?}",
        statuses(&settled, parked)
    );
}

/// #1019's falsifier, second half: **a coalesced request answers for every
/// question it carried.**
///
/// Router coalescing folds the discovery atom into whatever else that session
/// is already asking for, so the first discovery pass really is sent as
/// `kinds:{3,10002}`. Settlement used to require `kinds == {10002}` exactly,
/// which no coalesced request can ever satisfy — so the engine asked the
/// question, got the answer, and declined to read it.
#[test]
fn a_coalesced_request_still_answers_the_relay_list_question_it_carried() {
    let author = Keys::generate();
    let recipient = Keys::generate();
    let outbox = RelayUrl::parse("wss://outbox-a.example").unwrap();
    let indexer = RelayUrl::parse("wss://indexer.example").unwrap();

    let directory = SharedDirectory::new();
    directory.learns_write(&author, &outbox);
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(directory), 10);
    activate(&mut core, &author);
    connect(&mut core, 0, &indexer);

    let mentions = draft(21, "hello you").tag(nostr::Tag::public_key(recipient.public_key()));
    let (parked, opened) = publish_and_sign(&mut core, &author, mentions);
    assert_eq!(
        routed(&opened, parked).map(|(_, complete)| complete),
        Some(false),
        "an Unknown recipient keeps the obligation alive: {:?}",
        statuses(&opened, parked)
    );

    // A kind:3 demand for the same author is what coalescing folds the
    // discovery atom into -- the exact shape a live engine sends on its very
    // first discovery pass, where the contact list and the relay list are
    // wanted for the same person at the same moment.
    let coalesced = core.handle(EngineMsg::Subscribe(literal_query(
        &[3],
        &recipient.public_key().to_hex(),
    )));
    let (sub, filter) = req_for_kind(&coalesced, &indexer, 10002);
    let sub = sub.clone();
    assert_eq!(
        filter.kinds.clone(),
        Some(BTreeSet::from([3, 10002])),
        "coalescing folds discovery into the wider request, which is the shape an equality \
         test on `kinds` silently refuses to settle off"
    );

    let settled = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&indexer),
        eose_frame(&wire_sub_string(&sub)),
    ));

    assert_eq!(
        routed(&settled, parked),
        Some((BTreeSet::from([outbox]), true)),
        "the request asked about the recipient's kind:10002 among other things, and the EOSE \
         answers everything it asked: {:?}",
        statuses(&settled, parked)
    );
}
