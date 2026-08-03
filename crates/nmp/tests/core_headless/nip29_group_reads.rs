use super::*;

// ---- #1123: NIP-29 reads stay pinned to their group hosts --------------
//
// Headless, zero-socket falsifiers for the read half of #1123
// (`features/groups/reads-through-the-one-door.feature`). Every `Demand`
// here is minted by the SAME pure `nmp_nip29::group_demand_at`/
// `group_records_at` constructors the `nmp::nip29` facade calls -- nothing
// here is a parallel or simplified re-implementation of the door.
//
// Provenance in this engine is not something a test has to dig for: which
// relay served a row is a field on the row itself (`Row::sources`/
// `RowDelta`), and which relay a query currently trusts, and how, is a field
// on `AcquisitionEvidence` (`crates/nmp/src/core/evidence.rs`) -- both
// already flow through the ordinary `EmitRows`/subscription path every other
// headless test in this suite reads. Nothing below reaches past that.

const GROUP_KIND: u16 = 9;

fn group_query(host: &RelayUrl, group_id: &str, kinds: &[u16]) -> LiveQuery {
    let demand = nmp_nip29::group_demand_at(
        host,
        group_id,
        Filter {
            kinds: Some(kinds.iter().copied().collect()),
            ..Filter::default()
        },
    )
    .expect("a plain kind selection scopes cleanly");
    LiveQuery::single(demand)
}

fn h_tagged_event(signer: &Keys, group_id: &str, created_at: u64) -> nostr::Event {
    UnsignedEvent::new(
        signer.public_key(),
        Timestamp::from(created_at),
        Kind::from(GROUP_KIND),
        vec![nostr::Tag::parse(["h", group_id]).expect("h row parses")],
        format!("in {group_id}"),
    )
    .sign_with_keys(signer)
    .expect("fixture keys sign cleanly")
}

fn evidence_of(effects: &[Effect]) -> Vec<AcquisitionEvidence> {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(_, _, evidence) => Some(evidence.clone()),
            _ => None,
        })
        .expect("subscribe emits its initial acquisition evidence")
}

fn rows_of(effects: &[Effect]) -> Vec<RowDelta> {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(_, rows, _) => Some(rows.clone()),
            _ => None,
        })
        .expect("subscribe emits its initial row snapshot")
}

/// PROTOCOL-READSTHROUGHTHEONEDOOR-005 (witness half): two DIFFERENT group
/// ids on the SAME host, proven at the wire rather than only in the minted
/// `Demand` shape (`nmp::nip29::mod`'s
/// `two_group_ids_on_one_host_differ_only_in_their_h_branch` already proves
/// the shape). One host serves BOTH groups' own kind:9 content; a listing
/// scoped to "photographers" must surface only the "photographers" event
/// even though the other event arrives on the exact same connected session.
#[test]
fn two_group_ids_on_the_same_host_stay_separated_by_h_at_the_wire() {
    let host = RelayUrl::parse("wss://relay.groups.example").unwrap();
    let mut core = new_core(FixtureRoutingFacts::new());
    connect(&mut core, 0, &host);

    let effects = core.handle(EngineMsg::Subscribe(group_query(
        &host,
        "photographers",
        &[GROUP_KIND],
    )));
    let (sub_id, _) = req_for(&effects, &host);
    let sub = wire_sub_string(sub_id);

    let signer = Keys::generate();
    let mine = h_tagged_event(&signer, "photographers", 1_700_000_000);
    let theirs = h_tagged_event(&signer, "darkroom", 1_700_000_001);

    // Both events arrive on the SAME session/subscription id -- the only
    // thing separating them is the h row the store's own filter match reads,
    // exactly as a real relay answering one REQ would deliver both if it
    // ignored `#h` (it must not; NIP-29 relays enforce `#h` server-side too,
    // but this proves NMP's own local filter never depends on that).
    let after_mine = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&host),
        event_frame(&sub, mine.clone()),
    ));
    let after_theirs = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&host),
        event_frame(&sub, theirs.clone()),
    ));

    let delivered_mine = rows_of(&after_mine);
    assert!(
        delivered_mine
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == mine.id)),
        "the photographers-scoped query must surface its own event: {delivered_mine:?}"
    );

    assert!(
        !after_theirs
            .iter()
            .any(|effect| matches!(effect, Effect::EmitRows(_, rows, _) if !rows.is_empty())),
        "darkroom's own event must never surface on the photographers-scoped subscription: \
         {after_theirs:?}"
    );

    core.handle(EngineMsg::Unsubscribe(subscribed_handle(&effects)));
}

/// PROTOCOL-READSTHROUGHTHEONEDOOR-006: a group read's host comes ONLY from
/// the retained relay scope. A real, resolvable author outbox routing fact
/// for the SAME identity sits in the routing directory the whole time; a
/// group read must never widen onto it, because `group_demand_at` mints
/// `SourceAuthority::Pinned`, never `AuthorOutboxes`, and the router cannot
/// add a source a `Pinned` demand did not name.
#[test]
fn a_group_read_never_widens_beyond_its_pinned_host_to_a_discovered_author_outbox() {
    let author = Keys::generate();
    let group_host = RelayUrl::parse("wss://relay.groups.example").unwrap();
    let outbox = RelayUrl::parse("wss://alice-write.example").unwrap();
    let dir =
        FixtureRoutingFacts::new().with_outbound_routes(author.public_key(), [outbox.clone()]);
    let mut core = new_core(dir);
    activate(&mut core, &author);

    let effects = core.handle(EngineMsg::Subscribe(group_query(
        &group_host,
        "photographers",
        &[GROUP_KIND],
    )));

    let sessions: BTreeSet<RelayUrl> = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta.ops.iter().map(|(session, _)| session.relay.clone())),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        sessions,
        BTreeSet::from([group_host.clone()]),
        "a group read must open exactly its pinned host and never the author's own outbox, \
         even though the outbox is a real, resolvable routing fact for the active identity"
    );
    assert!(
        !sessions.contains(&outbox),
        "the discovered author outbox must never be contacted for a group read"
    );

    // The evidence surface names the same single source -- never a second,
    // outbox-sourced entry silently riding along.
    let evidence = evidence_of(&effects);
    let relays: BTreeSet<RelayUrl> = evidence
        .iter()
        .flat_map(|entry| entry.sources.iter())
        .map(|source| source.relay.clone())
        .collect();
    assert_eq!(relays, BTreeSet::from([group_host]));
}

/// PROTOCOL-READSTHROUGHTHEONEDOOR-007: an unproven host is never presented
/// as an authoritative empty group. A fresh subscription's session has not
/// yet completed a connection at all (no `EngineMsg::RelayConnected` was
/// ever delivered for it) -- the honest per-source fact for that is
/// `SourceStatus::Connecting` with `reconciled_through: None`
/// (`crates/nmp/src/core/evidence.rs`'s own documented vocabulary), which is
/// exactly what an app reads to tell "nothing here yet" apart from "proven
/// empty". The row set is empty, but `shortfall` stays empty too: there IS a
/// real covering source, so this is not the separate "nothing is even trying"
/// fact either.
#[test]
fn an_unproven_host_never_presents_a_group_read_as_authoritatively_empty() {
    let host = RelayUrl::parse("wss://relay.groups.example").unwrap();
    let mut core = new_core(FixtureRoutingFacts::new());
    // Deliberately no `connect(...)`: the session has a planned request but
    // has never completed a connection, the same shape
    // `crates/nmp/tests/core_headless/write_delivery.rs`'s
    // `an_unreachable_explicit_relay_is_accepted_because_the_door_cannot_know`
    // and `crates/nmp/tests/freshness.rs`'s
    // `nested_max_age_uses_inner_scoped_coverage_only` both use for the same
    // "never yet connected" fact on the read side.

    let effects = core.handle(EngineMsg::Subscribe(group_query(
        &host,
        "photographers",
        &[GROUP_KIND],
    )));

    let rows = rows_of(&effects);
    assert!(rows.is_empty(), "no row has ever been proven: {rows:?}");

    let evidence = evidence_of(&effects);
    assert_eq!(evidence.len(), 1, "one query, one evidence entry");
    assert!(
        evidence[0].shortfall.is_empty(),
        "a real covering source exists, so this must not ALSO read as \"nothing is even \
         trying\": {:?}",
        evidence[0].shortfall
    );
    let source = evidence[0]
        .sources
        .iter()
        .find(|source| source.relay == host)
        .expect("the pinned host must still name a covering source");
    assert_eq!(
        source.status,
        SourceStatus::Connecting,
        "a host that has never completed a connection is honestly \"connecting\", never \
         silently promoted to a proven or complete state"
    );
    assert_eq!(
        source.reconciled_through, None,
        "no watermark exists yet -- the empty row set above is not authoritative"
    );
}
