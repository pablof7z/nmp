//! #1233 -- what the group-records reader actually delivers, over real
//! sockets against real in-process relays.
//!
//! Every claim here is about a rule that could plausibly be got wrong in a
//! way that still compiles and still looks right on screen:
//!
//!   1. the lists UNION across hosts and the union is seeded from EVERY host
//!      that published one, so a subject listed solely by the second relay
//!      still appears, attributed to that relay alone --
//!      `a_subject_listed_only_by_the_second_host_still_appears_attributed_to_it`;
//!   2. the metadata does NOT merge field-wise: one host's whole record wins
//!      on `created_at`, and a field the winner left absent stays absent
//!      rather than being filled in from the loser --
//!      `metadata_is_one_hosts_whole_record_never_a_field_wise_merge`;
//!   3. a role the relay wrote survives and a role it did not write stays
//!      absent, on the admin list, with the members beside it unaffected --
//!      `an_admin_with_no_role_is_not_reported_as_a_member`;
//!   4. `differs` says whether the hosts disagree, and says no when they
//!      agree -- `differs_answers_the_dig_in_question_both_ways`;
//!   5. the group-scoped door delivers exactly one snapshot for the id it was
//!      narrowed to, before any record exists, so there is always something
//!      to render -- `the_group_scoped_door_delivers_one_snapshot_from_the_first_delivery`.
//!
//! Same version-shadowing precaution as every other integration test here:
//! never `use nostr_relay_builder::prelude::*`; `nmp-test-support` owns the
//! bridge between the two pinned `nostr` versions.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use nmp::nip29::{
    self, member_list_includes, GroupAvailability, GroupObservation, GroupRecord, GroupSnapshot,
};
use nmp::{Binding, Engine, EngineConfig};
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::{Keys, Kind, Tag, Timestamp, UnsignedEvent};

const GROUP_ID: &str = "photographers";
const METADATA: u16 = 39000;
const ADMINS: u16 = 39001;
const MEMBERS: u16 = 39002;

/// Long enough for a real round trip on a loaded CI runner, short enough that
/// a genuine failure reports rather than hangs.
const SETTLE: Duration = Duration::from_secs(20);
/// After the wanted state first holds, keep draining this long so a LATE,
/// WRONG delivery has a real chance to arrive before a negative assertion.
const QUIET: Duration = Duration::from_millis(500);

fn bare_engine() -> Engine {
    Engine::new(EngineConfig {
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        ..EngineConfig::default()
    })
    .expect("an in-memory engine builds")
}

/// A record signed by the relay's own key, exactly as NIP-29 says these are
/// produced. `signer` stands in for one host's own key: two hosts signing the
/// same group id produce two distinct events, which is the whole reason a
/// cross-host aggregate has to decide something.
fn relay_signed(signer: &Keys, kind: u16, created_at: u64, rows: Vec<Vec<&str>>) -> nostr::Event {
    UnsignedEvent::new(
        signer.public_key(),
        Timestamp::from(created_at),
        Kind::from(kind),
        rows.into_iter()
            .map(|row| Tag::parse(row).expect("a well-formed row"))
            .collect::<Vec<Tag>>(),
        String::new(),
    )
    .sign_with_keys(signer)
    .expect("fixture keys sign cleanly")
}

async fn wait_for(
    watching: &GroupObservation,
    pred: impl Fn(&GroupSnapshot) -> bool,
) -> GroupSnapshot {
    let deadline = Instant::now() + SETTLE;
    let mut seen: Vec<GroupSnapshot> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "no delivery satisfied the predicate; last saw {seen:?}"
        );
        match watching.next_within(remaining).await {
            Ok(Some(snapshots)) => {
                seen.clone_from(&snapshots);
                if let Some(found) = snapshots.into_iter().find(&pred) {
                    return found;
                }
            }
            other => panic!("the observation ended before the predicate held: {other:?}"),
        }
    }
}

async fn settle(watching: &GroupObservation, mut current: GroupSnapshot) -> GroupSnapshot {
    while let Ok(Some(snapshots)) = watching.next_within(QUIET).await {
        if let Some(found) = snapshots
            .into_iter()
            .find(|snapshot| snapshot.id == current.id)
        {
            current = found;
        }
    }
    current
}

// ===========================================================================
// 1. The union, and where it is seeded from.
// ===========================================================================

/// Two hosts, two member lists, one subject each. NIP-29 authority is
/// per-relay and inclusion in an observed list is evidence, so the honest
/// aggregate is a union: it asserts only true positives. Requiring agreement
/// would leave the value perpetually absent, because two relays' member sets
/// are essentially never identical -- as here.
///
/// The failure this catches is subtle and silent: an implementation that
/// seeds the union from the first host that answers and then intersects, or
/// that overwrites rather than merges, shows a roster missing exactly the
/// people only the second relay knows about, and looks completely normal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subject_listed_only_by_the_second_host_still_appears_attributed_to_it() {
    let host_a_signer = Keys::generate();
    let host_b_signer = Keys::generate();
    let only_at_a = Keys::generate().public_key();
    let only_at_b = Keys::generate().public_key();
    let at_both = Keys::generate().public_key();

    let host_a = ScriptedRelay::start(&RelayConfig::default()).await;
    let host_b = ScriptedRelay::start(&RelayConfig::default()).await;

    host_a
        .seed_signed_event(&relay_signed(
            &host_a_signer,
            MEMBERS,
            1_700_000_000,
            vec![
                vec!["d", GROUP_ID],
                vec!["p", &only_at_a.to_hex()],
                vec!["p", &at_both.to_hex()],
            ],
        ))
        .await;
    host_b
        .seed_signed_event(&relay_signed(
            &host_b_signer,
            MEMBERS,
            1_700_000_000,
            vec![
                vec!["d", GROUP_ID],
                vec!["p", &only_at_b.to_hex()],
                vec!["p", &at_both.to_hex()],
            ],
        ))
        .await;

    let engine = bare_engine();
    let group = nip29::group([host_a.url.clone(), host_b.url.clone()], GROUP_ID)
        .expect("two hosts form a scope");
    let watching = group
        .observe(&engine, [GroupRecord::Members])
        .expect("the records observation opens");

    let snapshot = wait_for(&watching, |snapshot| snapshot.per_host.len() == 2).await;
    let snapshot = settle(&watching, snapshot).await;

    let named: BTreeSet<nostr::PublicKey> = snapshot
        .members
        .iter()
        .map(|subject| subject.pubkey)
        .collect();
    assert_eq!(
        named,
        BTreeSet::from([only_at_a, only_at_b, at_both]),
        "the union must name every subject either relay listed, including the one only the \
         SECOND relay listed"
    );

    let hosts_of = |pubkey: nostr::PublicKey| {
        snapshot
            .members
            .iter()
            .find(|subject| subject.pubkey == pubkey)
            .map(|subject| subject.hosts.clone())
            .expect("the union names this subject")
    };
    assert_eq!(
        hosts_of(only_at_a),
        BTreeSet::from([host_a.url.clone()]),
        "a subject only host A listed is attributed to host A alone"
    );
    assert_eq!(
        hosts_of(only_at_b),
        BTreeSet::from([host_b.url.clone()]),
        "a subject only host B listed is attributed to host B alone"
    );
    assert_eq!(
        hosts_of(at_both),
        BTreeSet::from([host_a.url.clone(), host_b.url.clone()]),
        "a subject BOTH relays listed is attributed to both -- the attribution is what makes \
         the union honest"
    );

    // The dig-in beside the aggregate: exactly what each relay signed.
    assert_eq!(
        snapshot
            .at(&host_a.url)
            .and_then(|records| records.members.as_ref())
            .map(|record| record.subjects.len()),
        Some(2),
        "host A's own record names two subjects and is not folded with host B's"
    );
    assert!(
        snapshot.differs(GroupRecord::Members),
        "the relays disagree about who is a member, and an app must be able to learn that"
    );

    engine.shutdown();
    host_a.shutdown();
    host_b.shutdown();
}

// ===========================================================================
// 2. Metadata: event-wise latest wins, NEVER field-wise.
// ===========================================================================

/// Host A signs the NEWER record and gives it a name but no `about`. Host B
/// signs an OLDER record with both a (different) name and an `about`.
///
/// The honest answer is host A's whole record: a name from A, and no `about`
/// at all. A field-wise merge would show A's name beside B's `about` -- a
/// title and a description no relay ever signed together, rendered to a user
/// as though one had. That is the defect this asserts against, and it is
/// invisible on screen precisely because the result looks like a
/// well-populated group.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_is_one_hosts_whole_record_never_a_field_wise_merge() {
    let host_a_signer = Keys::generate();
    let host_b_signer = Keys::generate();

    let host_a = ScriptedRelay::start(&RelayConfig::default()).await;
    let host_b = ScriptedRelay::start(&RelayConfig::default()).await;

    let newer = relay_signed(
        &host_a_signer,
        METADATA,
        1_700_000_500,
        vec![vec!["d", GROUP_ID], vec!["name", "Photographers"]],
    );
    let older = relay_signed(
        &host_b_signer,
        METADATA,
        1_700_000_000,
        vec![
            vec!["d", GROUP_ID],
            vec!["name", "Darkroom regulars"],
            vec!["about", "an about row only the OLDER record carries"],
        ],
    );
    host_a.seed_signed_event(&newer).await;
    host_b.seed_signed_event(&older).await;

    let engine = bare_engine();
    let group = nip29::group([host_a.url.clone(), host_b.url.clone()], GROUP_ID)
        .expect("two hosts form a scope");
    let watching = group
        .observe(&engine, [GroupRecord::Metadata])
        .expect("the records observation opens");

    let snapshot = wait_for(&watching, |snapshot| snapshot.per_host.len() == 2).await;
    let snapshot = settle(&watching, snapshot).await;

    let metadata = snapshot.metadata.as_ref().expect("a record is shown");
    assert_eq!(
        metadata.event_id, newer.id,
        "the LATER created_at wins, entire"
    );
    assert_eq!(metadata.host, host_a.url, "and its host is named");
    assert_eq!(metadata.name.as_deref(), Some("Photographers"));
    assert_eq!(
        metadata.about, None,
        "the winning relay signed no about row, so there is no about row -- filling it in \
         from the losing relay would synthesize a record nobody signed"
    );
    assert_eq!(
        metadata.as_of,
        Timestamp::from(1_700_000_500u64),
        "as_of is the winning relay's own created_at, for display"
    );

    // The loser is not discarded, it is reachable.
    let at_b = snapshot
        .at(&host_b.url)
        .and_then(|records| records.metadata.as_ref())
        .expect("host B's own record stays reachable beside the aggregate");
    assert_eq!(at_b.event_id, older.id);
    assert_eq!(at_b.name.as_deref(), Some("Darkroom regulars"));
    assert_eq!(
        at_b.about.as_deref(),
        Some("an about row only the OLDER record carries")
    );
    assert!(
        snapshot.differs(GroupRecord::Metadata),
        "the two relays describe this group differently, and an app can learn that"
    );

    engine.shutdown();
    host_a.shutdown();
    host_b.shutdown();
}

// ===========================================================================
// 3. Roles: on the admin list, never invented.
// ===========================================================================

/// kind:39001 spells its rows `["p", pubkey, role]` and kind:39002 spells
/// them `["p", pubkey]`. A relay may leave the role position empty on an
/// admin row, and the shipped hand-rolled reader this replaces defaulted that
/// to `"member"` -- silently demoting an admin in the app's own model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_with_no_role_is_not_reported_as_a_member() {
    let host_signer = Keys::generate();
    let moderator = Keys::generate().public_key();
    let role_less_admin = Keys::generate().public_key();
    let plain_member = Keys::generate().public_key();

    let host = ScriptedRelay::start(&RelayConfig::default()).await;
    host.seed_signed_event(&relay_signed(
        &host_signer,
        ADMINS,
        1_700_000_000,
        vec![
            vec!["d", GROUP_ID],
            vec!["p", &moderator.to_hex(), "moderator"],
            vec!["p", &role_less_admin.to_hex()],
        ],
    ))
    .await;
    host.seed_signed_event(&relay_signed(
        &host_signer,
        MEMBERS,
        1_700_000_000,
        vec![vec!["d", GROUP_ID], vec!["p", &plain_member.to_hex()]],
    ))
    .await;

    let engine = bare_engine();
    let group = nip29::group([host.url.clone()], GROUP_ID).expect("one host forms a scope");
    let watching = group
        .observe(&engine, [GroupRecord::Admins, GroupRecord::Members])
        .expect("the records observation opens");

    let snapshot = wait_for(&watching, |snapshot| {
        snapshot.admins.len() == 2 && snapshot.members.len() == 1
    })
    .await;

    let role_of = |pubkey: nostr::PublicKey| {
        snapshot
            .admins
            .iter()
            .find(|subject| subject.pubkey == pubkey)
            .map(|subject| subject.role.clone())
            .expect("the admin list names this subject")
    };
    assert_eq!(role_of(moderator).as_deref(), Some("moderator"));
    assert_eq!(
        role_of(role_less_admin),
        None,
        "a relay that wrote no role must not be reported as having written one -- and above \
         all must not be silently recorded as a member"
    );
    assert!(
        !snapshot
            .members
            .iter()
            .any(|subject| subject.pubkey == role_less_admin),
        "the role-less ADMIN must not appear on the member list at all"
    );
    assert_eq!(snapshot.members[0].pubkey, plain_member);
    assert_eq!(snapshot.members[0].role, None);

    engine.shutdown();
    host.shutdown();
}

// ===========================================================================
// 4. differs, both ways.
// ===========================================================================

/// A dig-in affordance is only worth offering when there is something to dig
/// into. Two hosts publishing the SAME member list agree, so `differs` says
/// no -- an assertion that fails for an implementation that hard-codes
/// "more than one host means disagreement".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn differs_answers_the_dig_in_question_both_ways() {
    let host_a_signer = Keys::generate();
    let host_b_signer = Keys::generate();
    let member = Keys::generate().public_key();

    let host_a = ScriptedRelay::start(&RelayConfig::default()).await;
    let host_b = ScriptedRelay::start(&RelayConfig::default()).await;

    // Two DIFFERENT events (different relay keys), naming the same subject.
    for (signer, relay) in [(&host_a_signer, &host_a), (&host_b_signer, &host_b)] {
        relay
            .seed_signed_event(&relay_signed(
                signer,
                MEMBERS,
                1_700_000_000,
                vec![vec!["d", GROUP_ID], vec!["p", &member.to_hex()]],
            ))
            .await;
    }

    let engine = bare_engine();
    let group = nip29::group([host_a.url.clone(), host_b.url.clone()], GROUP_ID)
        .expect("two hosts form a scope");
    let watching = group
        .observe(&engine, [GroupRecord::Members])
        .expect("the records observation opens");

    let snapshot = wait_for(&watching, |snapshot| snapshot.per_host.len() == 2).await;
    let snapshot = settle(&watching, snapshot).await;

    assert_eq!(snapshot.members.len(), 1);
    assert_eq!(
        snapshot.members[0].hosts,
        BTreeSet::from([host_a.url.clone(), host_b.url.clone()])
    );
    assert!(
        !snapshot.differs(GroupRecord::Members),
        "both relays named exactly this subject, so there is nothing to dig into"
    );
    assert!(snapshot.disagreements.is_empty());

    engine.shutdown();
    host_a.shutdown();
    host_b.shutdown();
}

// ===========================================================================
// 5. The group-scoped door: one snapshot, from the first delivery.
// ===========================================================================

/// The room screen's path. There is no predicate, no collection and no id
/// lookup, and there is something to render before any record has arrived --
/// which is what makes `availability` usable as a spinner rather than as an
/// afterthought.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_group_scoped_door_delivers_one_snapshot_from_the_first_delivery() {
    let host_signer = Keys::generate();
    let host = ScriptedRelay::start(&RelayConfig::default()).await;

    let engine = bare_engine();
    let group = nip29::group([host.url.clone()], GROUP_ID).expect("one host forms a scope");
    let watching = group
        .observe(&engine, [GroupRecord::Metadata, GroupRecord::Members])
        .expect("the records observation opens");

    let first = watching
        .next_within(SETTLE)
        .await
        .expect("a delivery arrives")
        .expect("the observation is open");
    assert_eq!(first.len(), 1, "a group-scoped door delivers exactly one");
    assert_eq!(first[0].id, GROUP_ID);
    assert_eq!(
        first[0].metadata, None,
        "nothing has been published for this group, and nothing is invented"
    );
    assert!(first[0].members.is_empty());

    // An empty member list is delivered as an empty member list. What that
    // MEANS -- restricted, not yet published, genuinely empty -- is the app's
    // call, and NMP does not decide it here.
    host.seed_signed_event(&relay_signed(
        &host_signer,
        METADATA,
        1_700_000_000,
        vec![vec!["d", GROUP_ID], vec!["name", "Photographers"]],
    ))
    .await;

    let named = wait_for(&watching, |snapshot| snapshot.metadata.is_some()).await;
    assert_eq!(
        named.metadata.as_ref().and_then(|r| r.name.as_deref()),
        Some("Photographers")
    );
    assert_ne!(
        named.availability,
        GroupAvailability::SourceUnavailable,
        "a reachable host that answered is not a source failure"
    );

    engine.shutdown();
    host.shutdown();
}

// ===========================================================================
// The predicate door still finds groups by evidence, and the id set works.
// ===========================================================================

/// The scope-wide door with a composed predicate: the groups whose member
/// list names me, PLUS the ones I pinned by id. Both leaves resolve in one
/// observation, and each matching group gets its own snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_composed_predicate_delivers_one_snapshot_per_matching_group() {
    let host_signer = Keys::generate();
    let me = Keys::generate().public_key();
    let host = ScriptedRelay::start(&RelayConfig::default()).await;

    // A group I am a member of, found by evidence.
    host.seed_signed_event(&relay_signed(
        &host_signer,
        MEMBERS,
        1_700_000_000,
        vec![vec!["d", "photographers"], vec!["p", &me.to_hex()]],
    ))
    .await;
    host.seed_signed_event(&relay_signed(
        &host_signer,
        METADATA,
        1_700_000_000,
        vec![vec!["d", "photographers"], vec!["name", "Photographers"]],
    ))
    .await;
    // A group I pinned by id and am not listed in.
    host.seed_signed_event(&relay_signed(
        &host_signer,
        METADATA,
        1_700_000_000,
        vec![vec!["d", "darkroom"], vec!["name", "Darkroom"]],
    ))
    .await;

    let engine = bare_engine();
    let scope = nip29::on([host.url.clone()]).expect("one host forms a scope");
    let watching = scope
        .observe(
            &engine,
            member_list_includes(Binding::Literal(BTreeSet::from([me.to_hex()]))).union([
                nip29::any_of(Binding::Literal(BTreeSet::from(["darkroom".to_string()]))),
            ]),
            [GroupRecord::Metadata],
            None,
        )
        .expect("the records observation opens");

    let deadline = Instant::now() + SETTLE;
    let mut delivered: Vec<GroupSnapshot> = Vec::new();
    while Instant::now() < deadline && delivered.len() < 2 {
        match watching.next_within(QUIET).await {
            Ok(Some(snapshots)) => delivered = snapshots,
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    assert_eq!(
        delivered
            .iter()
            .map(|snapshot| snapshot.id.clone())
            .collect::<Vec<_>>(),
        vec!["darkroom".to_string(), "photographers".to_string()],
        "one snapshot per matching group, in group-id order: the evidence leaf found one and \
         the literal-id leaf found the other"
    );
    assert_eq!(
        delivered[1]
            .metadata
            .as_ref()
            .and_then(|record| record.name.as_deref()),
        Some("Photographers")
    );

    engine.shutdown();
    host.shutdown();
}
