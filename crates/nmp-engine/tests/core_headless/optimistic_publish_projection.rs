use super::*;
use std::collections::BTreeMap;

// ---- #1182: how a locally accepted write projects -----------------------
//
// `crates/nmp/tests/optimistic_publish.rs` proves #1182's headline claims
// over real sockets: the row appears at acceptance claiming zero relays, an
// accepting host enters its provenance and a rejecting one never does, and
// the mechanism is general rather than a NIP-29 courtesy.
//
// This module covers the projection states that file cannot hold still,
// at the `EngineCore` level with zero I/O:
//
//   * every host refusing the write -- the one outcome under which the row
//     never acquires any provenance at all, ever;
//   * a query opened AFTER the write, and a second query opened alongside
//     the first -- the snapshot door rather than the delta door;
//   * a filter the write does not match -- "optimistic" must not mean
//     "shown everywhere";
//   * a restart with the write still in flight;
//   * and the case where the ONLY host that carried the event is outside a
//     pinned query's own host set -- ours stays, foreign stays out (#1191).
//
// Determinism is the point. Each of these is a statement about an exact
// intermediate state, and a socket test would be asserting against a race.

const OPTIMISTIC_KIND: u16 = 1;
/// A structurally different kind the write must never leak into.
const UNRELATED_KIND: u16 = 30_023;

/// One branch of an ordinary pinned, cache-strict read -- the shape #1173
/// made every per-relay-authority read use, expressed with no protocol
/// helper at all so nothing here is NIP-29-specific.
fn pinned_strict(hosts: &[RelayUrl], kind: u16) -> LiveQuery {
    let mut demand = nmp_grammar::Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([kind])),
            ..Filter::default()
        },
        ReadRouting::Explicit(hosts.to_vec())
    )
    .expect("a nonempty pinned set with a non-outbox source is constructible");
    demand.cache = nmp_grammar::CacheMode::Strict;
    LiveQuery::single(demand)
}

fn signed_note(keys: &Keys, kind: u16, created_at: u64, content: &str) -> nostr::Event {
    UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(created_at),
        Kind::from_u16(kind),
        vec![],
        content.to_string(),
    )
    .sign_with_keys(keys)
    .expect("fixture keys sign cleanly")
}

/// Publish an already-signed event over an explicit route -- exactly what
/// `Group::publish_signed` and any `Explicit` app publish do, with no signer
/// round trip in the way of the acceptance moment being observed.
fn publish_signed_explicit(
    core: &mut EngineCore,
    event: nostr::Event,
    relays: impl IntoIterator<Item = RelayUrl>,
) -> (ReceiptId, Vec<Effect>) {
    let effects = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Signed(event),
        routing: WriteRouting::Explicit(Vec::from_iter(relays)),
        identity: Identity::Active,
    }));
    let id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitReceipt(id, _) => Some(*id),
            _ => None,
        })
        .expect("every accepted publish emits a receipt");
    (id, effects)
}

/// The row set one handle currently projects, folded from every `EmitRows`
/// batch addressed to it, plus how many times each row was announced as new.
#[derive(Default)]
struct Projected {
    rows: BTreeMap<nostr::EventId, nmp_grammar::Row>,
    added: BTreeMap<nostr::EventId, usize>,
}

impl Projected {
    fn fold(&mut self, effects: &[Effect], handle: ObservationId) {
        for effect in effects {
            let Effect::EmitRows(id, deltas, _) = effect else {
                continue;
            };
            if *id != handle {
                continue;
            }
            for delta in deltas {
                match delta {
                    RowDelta::Added(row) => {
                        *self.added.entry(row.id()).or_default() += 1;
                        self.rows.insert(row.id(), row.clone());
                    }
                    RowDelta::Updated(row) => {
                        self.rows.insert(row.id(), row.clone());
                    }
                    RowDelta::SourcesGrew { id, sources } => {
                        if let Some(row) = self.rows.get_mut(id) {
                            row.sources = sources.clone();
                        }
                    }
                    RowDelta::Removed(id) => {
                        self.rows.remove(id);
                    }
                }
            }
        }
    }

    fn sources_of(&self, id: &nostr::EventId) -> Option<&BTreeSet<RelayUrl>> {
        self.rows.get(id).map(|row| &row.sources)
    }

    fn row(&self, id: &nostr::EventId) -> Option<&nmp_grammar::Row> {
        self.rows.get(id)
    }

    fn shown(&self) -> Vec<(nostr::EventId, BTreeSet<RelayUrl>)> {
        self.rows
            .values()
            .map(|row| (row.id(), row.sources.clone()))
            .collect()
    }
}

/// One host's own per-relay verdict on the write.
fn verdict(
    core: &mut EngineCore,
    slot: u32,
    relay: &RelayUrl,
    event: &nostr::Event,
    accepted: bool,
    message: &str,
) -> Vec<Effect> {
    core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot,
            generation: 1,
        },
        signer_session(relay, event.pubkey),
        RelayFrame::from(RelayMessage::ok(event.id, accepted, message)),
    ))
}

/// Open a handle and fold its initial snapshot in one step.
fn open(core: &mut EngineCore, query: LiveQuery) -> (ObservationId, Projected, Vec<Effect>) {
    let effects = core.handle_and_flush(EngineMsg::Subscribe(query));
    let handle = subscribed_handle(&effects);
    let mut projected = Projected::default();
    projected.fold(&effects, handle);
    (handle, projected, effects)
}

// =========================================================================
// Every host refuses it. The row is still the user's, and still shown.
// =========================================================================

/// The outcome `optimistic_publish.rs` never reaches: not "no answer yet"
/// (its two unreachable hosts) and not "one host took it" (its accept/reject
/// pair), but every host in the route explicitly REFUSING. That is the only
/// case in which the row's provenance is empty *permanently* rather than
/// momentarily, so it is the only case that asks whether an empty source set
/// means "not yet" or "never".
///
/// It must mean neither, as far as visibility goes. The rejections are
/// carried on the receipt, per host, in each host's own words; the row stays
/// exactly where the user put it. A feed that silently deleted the message
/// would be telling the user something the receipt is already telling them
/// better, and destroying their text to say it.
#[test]
fn an_event_every_host_refused_stays_visible_reporting_zero_relays() {
    let me = Keys::generate();
    let host_a = RelayUrl::parse("wss://refuse-a.example").unwrap();
    let host_b = RelayUrl::parse("wss://refuse-b.example").unwrap();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &me);
    connect_signer(&mut core, 0, &host_a, me.public_key());
    connect_signer(&mut core, 1, &host_b, me.public_key());
    authenticate_signer(&mut core, 0, &host_a, &me);
    authenticate_signer(&mut core, 1, &host_b, &me);

    let pin = [host_a.clone(), host_b.clone()];
    let (handle, mut projected, _) = open(&mut core, pinned_strict(&pin, OPTIMISTIC_KIND));

    let event = signed_note(&me, OPTIMISTIC_KIND, 900, "every host refuses this");
    let (_receipt, accepted) = publish_signed_explicit(&mut core, event.clone(), pin.clone());
    projected.fold(&accepted, handle);
    assert_eq!(
        projected.sources_of(&event.id),
        Some(&BTreeSet::new()),
        "before any host has answered, the row is shown claiming zero relays: {:?}",
        projected.shown()
    );

    mark_written(&mut core, &accepted, &host_a);
    mark_written(&mut core, &accepted, &host_b);
    let first = verdict(
        &mut core,
        0,
        &host_a,
        &event,
        false,
        "blocked: refused by a",
    );
    let second = verdict(
        &mut core,
        1,
        &host_b,
        &event,
        false,
        "blocked: refused by b",
    );
    projected.fold(&first, handle);
    projected.fold(&second, handle);

    let statuses: Vec<WriteFact> = receipt_statuses(&first)
        .into_iter()
        .chain(receipt_statuses(&second))
        .collect();
    assert!(
        statuses
            .iter()
            .any(|s| matches!(s, WriteFact::Relay { relay: r, state: RelayState::Rejected { reason: m }, .. } if r == &host_a && m.contains("a"))),
        "host A's refusal is an ordinary per-relay receipt fact: {statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .any(|s| matches!(s, WriteFact::Relay { relay: r, state: RelayState::Rejected { reason: m }, .. } if r == &host_b && m.contains("b"))),
        "host B's refusal is an ordinary per-relay receipt fact: {statuses:?}"
    );

    assert_eq!(
        projected.sources_of(&event.id),
        Some(&BTreeSet::new()),
        "a universally refused write is still the user's own accepted event: it \
         stays in the feed, still claiming zero relays. The refusals live on the \
         receipt, which is where an app reads them. saw {:?}",
        projected.shown()
    );

    // And the same answer through the snapshot door, not only the delta one.
    let (_, fresh, _) = open(&mut core, pinned_strict(&pin, OPTIMISTIC_KIND));
    assert_eq!(
        fresh.sources_of(&event.id),
        Some(&BTreeSet::new()),
        "a query opened after every host refused sees the same row: {:?}",
        fresh.shown()
    );
}

// =========================================================================
// The snapshot door, and two apps watching at once.
// =========================================================================

/// An app that presses send and THEN navigates to the feed must see the
/// message, and so must a second feed that was already open. Both are the
/// ordinary case and they travel different code paths: the already-open
/// handle is served by the delta door, the newly-opened one by the initial
/// snapshot. A mechanism that only pushed the row to whichever subscription
/// happened to be live when the write landed would pass the socket
/// falsifiers and fail every app that opens a screen after sending.
#[test]
fn a_query_opened_after_the_write_sees_it_exactly_as_one_already_open_does() {
    let me = Keys::generate();
    let host = RelayUrl::parse("wss://snapshot-door.example").unwrap();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &me);

    let pin = [host.clone()];
    let (already_open, mut before, _) = open(&mut core, pinned_strict(&pin, OPTIMISTIC_KIND));
    let (alongside, mut second, _) = open(&mut core, pinned_strict(&pin, OPTIMISTIC_KIND));

    let event = signed_note(&me, OPTIMISTIC_KIND, 901, "sent, then the feed is opened");
    let (_receipt, accepted) = publish_signed_explicit(&mut core, event.clone(), pin.clone());
    before.fold(&accepted, already_open);
    second.fold(&accepted, alongside);

    assert_eq!(
        before.sources_of(&event.id),
        Some(&BTreeSet::new()),
        "the feed that was already open shows it: {:?}",
        before.shown()
    );
    assert_eq!(
        second.sources_of(&event.id),
        Some(&BTreeSet::new()),
        "a SECOND feed on the same selection shows it too -- visibility is a \
         property of the store, not a courtesy to the publishing subscription: {:?}",
        second.shown()
    );
    assert_eq!(
        before.added.get(&event.id).copied(),
        Some(1),
        "one row, announced once"
    );

    let (_, opened_later, _) = open(&mut core, pinned_strict(&pin, OPTIMISTIC_KIND));
    assert_eq!(
        opened_later.sources_of(&event.id),
        Some(&BTreeSet::new()),
        "a feed opened AFTER the write shows the same row with the same honest \
         provenance -- the write is in the store, not merely in flight to an \
         open subscription: {:?}",
        opened_later.shown()
    );
}

// =========================================================================
// Optimistic is not "shown everywhere".
// =========================================================================

/// The guard on the whole mechanism. "Show a locally accepted write
/// immediately" is scoped by the same filter matching every other row obeys;
/// it is emphatically not "show every locally accepted write in every
/// query". An implementation that special-cased zero-provenance rows into
/// visibility without re-checking the filter would satisfy every other
/// falsifier in this file and put the user's note into an unrelated feed.
#[test]
fn a_locally_accepted_write_never_enters_a_query_its_filter_excludes() {
    let me = Keys::generate();
    let host = RelayUrl::parse("wss://filter-honesty.example").unwrap();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &me);

    let pin = [host.clone()];
    let (matching, mut shown, _) = open(&mut core, pinned_strict(&pin, OPTIMISTIC_KIND));
    let (unrelated, mut hidden, _) = open(&mut core, pinned_strict(&pin, UNRELATED_KIND));

    let event = signed_note(&me, OPTIMISTIC_KIND, 902, "a note, not an article");
    let (_receipt, accepted) = publish_signed_explicit(&mut core, event.clone(), pin.clone());
    shown.fold(&accepted, matching);
    hidden.fold(&accepted, unrelated);

    assert_eq!(
        shown.sources_of(&event.id),
        Some(&BTreeSet::new()),
        "the query whose filter it matches shows it: {:?}",
        shown.shown()
    );
    assert!(
        hidden.rows.is_empty(),
        "a query the write does not match must not show it -- optimistic \
         visibility is filter-scoped like everything else: {:?}",
        hidden.shown()
    );

    let (_, fresh_unrelated, _) = open(&mut core, pinned_strict(&pin, UNRELATED_KIND));
    assert!(
        fresh_unrelated.rows.is_empty(),
        "and the snapshot door agrees: {:?}",
        fresh_unrelated.shown()
    );
}

// =========================================================================
// A restart with the write still in flight.
// =========================================================================

/// The user sent a message and the app was killed before any host answered.
/// On the next launch the message is still in the feed, still claiming zero
/// relays, because it is still a real obligation in the outbound publication
/// queue. Anything less loses the user's text on a crash while continuing to
/// try to publish it -- the row and the obligation would disagree.
///
/// A real reopen, per this codebase's established discipline: the store
/// handle is genuinely released and the file reopened, because `RedbStore`
/// permits one open handle per path per process.
#[test]
fn a_write_still_in_flight_is_still_in_the_feed_after_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("optimistic-restart.redb");
    let me = Keys::generate();
    let host = RelayUrl::parse("wss://restart-host.example").unwrap();
    let pin = [host.clone()];

    let event = signed_note(&me, OPTIMISTIC_KIND, 903, "sent, then the app died");

    {
        let store = RedbStore::open(&path).unwrap();
        let mut core =
            EngineCore::new_with_fixture_routing_facts(store, FixtureRoutingFacts::new(), 10);
        activate(&mut core, &me);
        let (handle, mut projected, _) = open(&mut core, pinned_strict(&pin, OPTIMISTIC_KIND));
        let (_receipt, accepted) = publish_signed_explicit(&mut core, event.clone(), pin.clone());
        projected.fold(&accepted, handle);
        assert_eq!(
            projected.sources_of(&event.id),
            Some(&BTreeSet::new()),
            "shown before the process dies: {:?}",
            projected.shown()
        );
        // No host ever answers. The handle and the store go away here.
    }

    let store = RedbStore::open(&path).unwrap();
    let mut core =
        EngineCore::new_with_fixture_routing_facts(store, FixtureRoutingFacts::new(), 10);
    let _ = core.recover_on_boot();
    activate(&mut core, &me);
    let (_, recovered, _) = open(&mut core, pinned_strict(&pin, OPTIMISTIC_KIND));
    assert_eq!(
        recovered.sources_of(&event.id),
        Some(&BTreeSet::new()),
        "after a real reopen the message is still in the feed, still claiming \
         zero relays -- the write is still owed: {:?}",
        recovered.shown()
    );
}

/// A pending row is not merely a process-local optimistic projection. The
/// canonical row persisted at acceptance remembers that its signature has not
/// arrived yet, so a cold query after restart must report `Pending` again --
/// never infer `Signed` merely because every stored row has an `Event` shape.
#[test]
fn an_unsigned_write_is_still_explicitly_pending_after_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pending-signature-restart.redb");
    let me = Keys::generate();
    let host = RelayUrl::parse("wss://pending-signature-restart.example").unwrap();
    let pin = [host.clone()];
    let expected_id;

    {
        let store = RedbStore::open(&path).unwrap();
        let mut core =
            EngineCore::new_with_fixture_routing_facts(store, FixtureRoutingFacts::new(), 10);
        activate(&mut core, &me);
        let (handle, mut projected, _) = open(&mut core, pinned_strict(&pin, OPTIMISTIC_KIND));
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(904, "accepted before its signer answers")),
            routing: WriteRouting::Explicit(pin.to_vec()),
            identity: Identity::Active,
        }));
        let (_, _, frozen) = find_sign_request(&accepted);
        expected_id = frozen.sign_with_keys(&me).unwrap().id;
        projected.fold(&accepted, handle);

        let row = projected
            .row(&expected_id)
            .expect("acceptance inserts the pending canonical row");
        assert_eq!(row.signature(), nmp_grammar::RowSignature::Pending);
        assert!(
            row.signed_event().is_none(),
            "the pending app row must not expose the storage sentinel"
        );
    }

    let store = RedbStore::open(&path).unwrap();
    let mut restarted =
        EngineCore::new_with_fixture_routing_facts(store, FixtureRoutingFacts::new(), 10);
    let _ = restarted.recover_on_boot();
    activate(&mut restarted, &me);
    let (_, recovered, _) = open(&mut restarted, pinned_strict(&pin, OPTIMISTIC_KIND));
    let row = recovered
        .row(&expected_id)
        .expect("the cold snapshot returns the accepted canonical row");
    assert_eq!(row.signature(), nmp_grammar::RowSignature::Pending);
    assert!(
        row.signed_event().is_none(),
        "restart must preserve pending without exposing the storage sentinel"
    );
}

// =========================================================================
// Ours versus foreign, never carried versus uncarried.
// =========================================================================

/// The place #1182 and #1173 meet, and the defect #1191 recorded there.
///
/// An app watching a strict subset of its own write route is ordinary: pin
/// the read to one host, publish to two. Under the shipped rule the row was
/// shown while nothing had carried it and WITHDRAWN the moment anything did,
/// so the answer to "is my message on screen" was decided by a host this
/// feed is not watching. With the watched host refusing either way, the
/// message survived if the other host stayed silent
/// (`an_event_every_host_refused_stays_visible_reporting_zero_relays`) and
/// vanished if the other host accepted it. The watched host did the same
/// thing in both cases, so the two answers cannot both be right.
///
/// The distinction the predicate was missing is ours versus foreign, not
/// carried versus uncarried. This row entered through the local write door
/// and keeps its local origin forever; what any relay does with it afterwards
/// changes its provenance, never its ownership. So it stays, and it reports
/// the truth: carried by `carrier`, a host this feed is not pinned to.
///
/// Governed as `WRITES-OPTIMISTICPUBLISH-010` in
/// `features/writes/optimistic-publish.feature`. Its foreign-data twin
/// `a_foreign_row_carried_only_outside_the_pin_is_still_invisible` is the
/// other half of the same rule and must stay green with it.
#[test]
fn the_users_own_row_survives_a_carrier_outside_the_pin_and_reports_it_honestly() {
    let me = Keys::generate();
    let carrier = RelayUrl::parse("wss://carrier.example").unwrap();
    let watched = RelayUrl::parse("wss://watched.example").unwrap();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &me);
    connect_signer(&mut core, 0, &carrier, me.public_key());
    connect_signer(&mut core, 1, &watched, me.public_key());
    authenticate_signer(&mut core, 0, &carrier, &me);
    authenticate_signer(&mut core, 1, &watched, &me);
    connect(&mut core, 2, &carrier);

    // The app watches ONLY `watched` while publishing to BOTH hosts. The
    // second handle exists so `carrier` has an open REQ to deliver under --
    // delivery is what writes relay provenance onto the row.
    let (live, mut watched_feed, _) = open(
        &mut core,
        pinned_strict(std::slice::from_ref(&watched), OPTIMISTIC_KIND),
    );
    let (_carrier_handle, _, opened_carrier) = open(
        &mut core,
        pinned_strict(std::slice::from_ref(&carrier), OPTIMISTIC_KIND),
    );

    let event = signed_note(
        &me,
        OPTIMISTIC_KIND,
        904,
        "carried only by the unwatched host",
    );
    let (_receipt, accepted) =
        publish_signed_explicit(&mut core, event.clone(), [carrier.clone(), watched.clone()]);
    watched_feed.fold(&accepted, live);
    assert_eq!(
        watched_feed.sources_of(&event.id),
        Some(&BTreeSet::new()),
        "#1182: before any host answers, the watched feed shows it: {:?}",
        watched_feed.shown()
    );

    mark_written(&mut core, &accepted, &carrier);
    mark_written(&mut core, &accepted, &watched);
    let acked = verdict(&mut core, 0, &carrier, &event, true, "");
    let refused = verdict(&mut core, 1, &watched, &event, false, "blocked: refused");
    watched_feed.fold(&acked, live);
    watched_feed.fold(&refused, live);
    assert!(
        receipt_statuses(&acked)
            .iter()
            .any(|s| matches!(s, WriteFact::Relay { relay: r, state: RelayState::Published, .. } if r == &carrier)),
        "the unwatched host took it"
    );
    assert!(
        receipt_statuses(&refused)
            .iter()
            .any(|s| matches!(s, WriteFact::Relay { relay: r, state: RelayState::Rejected { reason: _ }, .. } if r == &watched)),
        "the watched host refused it"
    );

    // The carrier now delivers the event back under its own REQ, which is
    // what actually writes `Provenance.seen`.
    let (sub_id, _) = req_for(&opened_carrier, &carrier);
    let sub = wire_sub_string(sub_id);
    let ingest = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 2,
            generation: 1,
        },
        public_session(&carrier),
        event_frame(&sub, event.clone()),
    ));
    watched_feed.fold(&ingest, live);

    assert_eq!(
        watched_feed.sources_of(&event.id),
        Some(&BTreeSet::from([carrier.clone()])),
        "#1191: the user's own message is NOT withdrawn because a host this \
         feed is not watching carried it -- it stays, and it names that host \
         rather than pretending nobody has it. saw {:?}",
        watched_feed.shown()
    );

    // And the snapshot door gives the same answer, so this is one rule and
    // not a delta-path artifact.
    let (_, fresh, _) = open(
        &mut core,
        pinned_strict(std::slice::from_ref(&watched), OPTIMISTIC_KIND),
    );
    assert_eq!(
        fresh.sources_of(&event.id),
        Some(&BTreeSet::from([carrier.clone()])),
        "a freshly opened identical feed agrees: {:?}",
        fresh.shown()
    );

    // The two rows of #1191's table now agree. `watched` refused in both, and
    // what `carrier` did decides nothing about visibility -- only about the
    // source set the row honestly reports.
    let (_, agnostic, _) = open(
        &mut core,
        LiveQuery::single(Demand {
            selection: Filter {
                kinds: Some(BTreeSet::from([OPTIMISTIC_KIND])),
                ..Filter::default()
            },
            ..Demand::default()
        }),
    );
    assert_eq!(
        agnostic.sources_of(&event.id),
        Some(&BTreeSet::from([carrier.clone()])),
        "an unpinned feed reports exactly the same provenance -- the pin \
         decides visibility, never what a visible row claims: {:?}",
        agnostic.shown()
    );
}

/// The isolation half of the same rule, in the same shape, so the fix above
/// cannot have been "show more rows under a pin".
///
/// A row this node never wrote, delivered only by `carrier`, must stay
/// invisible to a feed pinned to `watched` -- exactly what #1173 exists for.
/// The two tests differ in one fact only: who accepted the write. If ours
/// versus foreign were ever collapsed back into carried versus uncarried,
/// one of them goes red.
#[test]
fn a_foreign_row_carried_only_outside_the_pin_is_still_invisible() {
    let me = Keys::generate();
    let someone_else = Keys::generate();
    let carrier = RelayUrl::parse("wss://foreign-carrier.example").unwrap();
    let watched = RelayUrl::parse("wss://foreign-watched.example").unwrap();
    let mut core = new_core(FixtureRoutingFacts::new());
    activate(&mut core, &me);
    connect(&mut core, 2, &carrier);

    let (live, mut watched_feed, _) = open(
        &mut core,
        pinned_strict(std::slice::from_ref(&watched), OPTIMISTIC_KIND),
    );
    let (_carrier_handle, _, opened_carrier) = open(
        &mut core,
        pinned_strict(std::slice::from_ref(&carrier), OPTIMISTIC_KIND),
    );

    let theirs = signed_note(&someone_else, OPTIMISTIC_KIND, 905, "somebody else's note");
    let (sub_id, _) = req_for(&opened_carrier, &carrier);
    let sub = wire_sub_string(sub_id);
    let ingest = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 2,
            generation: 1,
        },
        public_session(&carrier),
        event_frame(&sub, theirs.clone()),
    ));
    watched_feed.fold(&ingest, live);

    assert!(
        watched_feed.rows.is_empty(),
        "#1173: a row only the unwatched host served never answers for the \
         watched one: {:?}",
        watched_feed.shown()
    );

    let (_, fresh, _) = open(
        &mut core,
        pinned_strict(std::slice::from_ref(&watched), OPTIMISTIC_KIND),
    );
    assert!(
        fresh.rows.is_empty(),
        "and the snapshot door agrees: {:?}",
        fresh.shown()
    );

    let (_, from_the_carrier, _) = open(
        &mut core,
        pinned_strict(std::slice::from_ref(&carrier), OPTIMISTIC_KIND),
    );
    assert_eq!(
        from_the_carrier.sources_of(&theirs.id),
        Some(&BTreeSet::from([carrier.clone()])),
        "the row is in the store and answers for the host that DID serve it -- \
         invisibility above is isolation, not absence: {:?}",
        from_the_carrier.shown()
    );
}
