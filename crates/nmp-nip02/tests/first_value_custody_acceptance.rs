//! First-value custody, restart survival, and source rebase for NIP-02,
//! proven through the direct Rust facade.
//!
//! These three scenarios were written in `nmp-parity`, which existed only to
//! run one loopback scenario through both the Rust facade and the FFI facade
//! and compare the two. Most of that crate was FFI-projection proof and died
//! with the FFI facade. These three are not: each drives ONE facade and
//! asserts engine behaviour -- what NMP does when a follow is accepted before
//! any relay has answered, what survives a close/reopen, and what happens
//! when the real kind:3 source finally arrives. Two of them happened to be
//! written against the FFI engine because that is where the harness lived;
//! nothing they assert is about the boundary. They are ported here, to the
//! crate that owns NIP-02's meaning, rather than deleted.
//!
//! What is deliberately NOT here: the identical-projection assertions
//! (`direct_and_ffi_*`), which compared two facades and have no subject with
//! one facade left.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nmp::{
    Binding, Demand, Derived, Engine, EngineConfig, Filter, LiveQuery, ReadRouting,
    ReceiptReattachment, ReceiptStream, RelayState, RelayWaiting, Row, RowDelta, Selector,
    SigningState, Subscription, WriteFact, WriteOutcome,
};
use nmp_nip02::{
    follow_capability, follow_writes, observe_following, set_following, FollowChange,
    FollowRelationship,
};
use nmp_store::{RedbStore, RelayObserved};
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::{EventBuilder, Keys, Kind, PublicKey, RelayUrl, Tag, Timestamp};

const WAIT: Duration = Duration::from_secs(30);
const QUERY_CREATED_AT: u64 = 1_700_000_100;

/// A relay that is reachable but never answers the author's kind:10002
/// question. Routing therefore never retires, which is exactly the state
/// every restart scenario below needs: the write stays in the publish queue
/// under its original receipt instead of settling and being retired.
const NONANSWERING_INDEXER: &str = "wss://indexer.example";

fn fixed_keys() -> Keys {
    Keys::parse("0000000000000000000000000000000000000000000000000000000000000001")
        .expect("fixed fixture key must parse")
}

fn outbox_provider() -> Option<Box<dyn nmp::AuthorRouteProvider>> {
    Some(Box::new(nmp_outbox::Nip65Outbox::new([
        RelayUrl::parse(NONANSWERING_INDEXER).expect("fixture indexer url parses")
    ])))
}

// ---------------------------------------------------------------------------
// Receipt-fact normalization.
//
// Ported unchanged from `nmp-parity`: the tests below assert on the NAMES of
// the write facts a follow action emits, and the two axes those facts move on
// (routing and delivery) are unordered against each other by construction.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum NormFollowActionStatus {
    Receipt(&'static str),
}

fn relay_state_name(state: &RelayState) -> &'static str {
    match state {
        RelayState::Waiting(RelayWaiting::NotConnected) => "awaiting_relay",
        RelayState::Waiting(RelayWaiting::NeedsAuth) => "awaiting_auth",
        RelayState::Waiting(RelayWaiting::Eligible { .. }) => "eligible",
        RelayState::Waiting(RelayWaiting::BackingOff { .. }) => "backing_off",
        RelayState::Attempting { .. } => "attempting",
        RelayState::Sent { .. } => "sent",
        RelayState::Published => "published",
        RelayState::Rejected { .. } => "rejected",
        RelayState::AuthFailed { .. } => "auth_failed",
        RelayState::GaveUp => "gave_up",
    }
}

fn follow_receipt_name(status: &WriteFact) -> &'static str {
    match status {
        WriteFact::Signing(SigningState::AwaitingSigner { .. }) => "awaiting_signer",
        WriteFact::Signing(SigningState::InFlight { .. }) => "signing_in_flight",
        WriteFact::Signing(SigningState::Signed { .. }) => "signed",
        WriteFact::Signing(SigningState::Refused { .. }) => "signing_refused",
        // `complete` is the routing AXIS's own terminal, and it is the only
        // thing that distinguishes "still discovering destinations" from
        // "this answer can never change again".
        WriteFact::Destinations { complete: true, .. } => "routed_complete",
        WriteFact::Destinations { .. } => "routed",
        WriteFact::Relay { state, .. } => relay_state_name(state),
        WriteFact::Outcome(WriteOutcome::Settled) => "settled",
        WriteFact::Outcome(WriteOutcome::NoDestination) => "no_destination",
        WriteFact::Outcome(WriteOutcome::NotSent(_)) => "not_sent",
        WriteFact::Outcome(WriteOutcome::Superseded) => "superseded_after_handoff",
        WriteFact::Outcome(WriteOutcome::Refused(_)) => "refused",
    }
}

fn is_delivery_terminal(status: &NormFollowActionStatus) -> bool {
    matches!(
        status,
        NormFollowActionStatus::Receipt(
            "published"
                | "rejected"
                | "auth_failed"
                | "gave_up"
                | "settled"
                | "no_destination"
                | "not_sent"
                | "superseded_after_handoff"
                | "refused"
                | "signing_refused"
        )
    )
}

/// Drain through the action's delivery terminal, or through `settled` when
/// `through_settled` is set.
///
/// `settled` is the signal that the coordinate's generation fully retired --
/// the exact condition the sequential-follow scenario depends on actually
/// being reached, not merely being possible.
fn collect_follow_action(
    receipt: ReceiptStream,
    through_settled: bool,
) -> Vec<NormFollowActionStatus> {
    let deadline = Instant::now() + WAIT;
    let mut result = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let status = match receipt.statuses.recv_timeout(remaining) {
            Ok(status) => status,
            Err(nmp::FifoRecvTimeoutError::Closed) if !through_settled => return result,
            Err(error) => panic!(
                "follow action did not reach its terminal within the total {WAIT:?} bound \
                 ({error:?}); seen={result:?}"
            ),
        };
        let normalized = NormFollowActionStatus::Receipt(follow_receipt_name(&status));
        let done = if through_settled {
            matches!(normalized, NormFollowActionStatus::Receipt("settled"))
        } else {
            is_delivery_terminal(&normalized)
        };
        result.push(normalized);
        if done {
            return result;
        }
    }
}

// ---------------------------------------------------------------------------
// Row observation.
// ---------------------------------------------------------------------------

fn pinned_contact_list(author: PublicKey, relay: RelayUrl) -> Demand {
    Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
            authors: Some(Binding::Literal(BTreeSet::from([author.to_hex()]))),
            ..Filter::default()
        },
        ReadRouting::Explicit(vec![relay]),
    )
    .expect("the contact-list source is pinned to one relay")
}

/// kind:1 notes whose authors are DERIVED from the `p` tags of the pinned
/// contact list above. This is the query whose recomputation the offline
/// scenario is about.
fn pinned_follow_feed(author: PublicKey, relay: RelayUrl) -> LiveQuery {
    let following = pinned_contact_list(author, relay.clone());
    let feed = Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([Kind::TextNote.as_u16()])),
            authors: Some(Binding::Derived(Box::new(Derived {
                inner: following,
                project: Selector::Tag("p".to_string()),
            }))),
            ..Filter::default()
        },
        ReadRouting::Explicit(vec![relay]),
    )
    .expect("the derived feed is pinned to the same relay");
    LiveQuery::single(feed)
}

fn apply_deltas(rows: &mut BTreeMap<String, Row>, deltas: Vec<RowDelta>) {
    for delta in deltas {
        match delta {
            RowDelta::Added(row) | RowDelta::Updated(row) => {
                rows.insert(row.id().to_hex(), row);
            }
            RowDelta::SourcesGrew { .. } => {}
            RowDelta::Removed(id) => {
                rows.remove(&id.to_hex());
            }
        }
    }
}

fn contact_authors(row: &Row) -> BTreeSet<String> {
    row.tags()
        .iter()
        .filter(|tag| tag.as_slice().first().is_some_and(|cell| cell == "p"))
        .filter_map(|tag| tag.as_slice().get(1).cloned())
        .collect()
}

/// Wait until the ONE visible kind:3 row carries exactly `expected_authors`,
/// the expected content, and the unrelated tag it must never have dropped.
/// Returns that row's id.
fn wait_for_contact_list(
    subscription: &Subscription,
    rows: &mut BTreeMap<String, Row>,
    expected_authors: &BTreeSet<String>,
    expected_content: &str,
    required_unrelated_tag: &[String],
    label: &str,
) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        let contact_lists = rows
            .values()
            .filter(|row| row.kind().as_u16() == Kind::ContactList.as_u16())
            .collect::<Vec<_>>();
        if contact_lists.len() == 1 {
            let row = contact_lists[0];
            if contact_authors(row) == *expected_authors
                && row.content() == expected_content
                && row
                    .tags()
                    .iter()
                    .any(|tag| tag.as_slice() == required_unrelated_tag)
            {
                return row.id().to_hex();
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let frame = subscription.recv_timeout(remaining).unwrap_or_else(|error| {
            let current = rows
                .values()
                .filter(|row| row.kind().as_u16() == Kind::ContactList.as_u16())
                .map(|row| (row.content().to_owned(), contact_authors(row)))
                .collect::<Vec<_>>();
            panic!(
                "{label} did not settle within the total {WAIT:?} bound: {error:?}; \
                 expected_authors={expected_authors:?}; current={current:?}"
            )
        });
        apply_deltas(rows, frame.deltas);
    }
}

fn wait_for_note_authors(
    subscription: &Subscription,
    rows: &mut BTreeMap<String, Row>,
    expected: &BTreeSet<String>,
    label: &str,
) {
    let deadline = Instant::now() + WAIT;
    loop {
        let authors = rows
            .values()
            .filter(|row| row.kind().as_u16() == Kind::TextNote.as_u16())
            .map(|row| row.pubkey().to_hex())
            .collect::<BTreeSet<_>>();
        if &authors == expected {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let frame = subscription.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!(
                "{label} did not settle within the total {WAIT:?} bound: {error:?}; \
                 expected={expected:?}; current={authors:?}"
            )
        });
        apply_deltas(rows, frame.deltas);
    }
}

// ---------------------------------------------------------------------------
// Scenarios.
// ---------------------------------------------------------------------------

/// #1692: a capability-default write to an already-settled replaceable
/// coordinate must not be refused as stale. Before the fix, `apply_plan`'s
/// no-prior-generation branch recognized only "nothing exists yet" (a genuine
/// first write); it had no arm for "the coordinate's real canonical event
/// exists, but `close_cohort` already retired its tracking row because the
/// first write's own generation fully settled" -- which is the ordinary shape
/// of `follow, wait, follow again`.
///
/// Both routing axes get a real, answering relay here, so settlement is
/// forced, not merely possible -- the assertion on `first_statuses` below is
/// the proof that this test exercises the real race and would fail loudly
/// (via timeout) if it stopped doing so.
///
/// This is a permanent acceptance test for the ordinary user story, not a
/// diagnostic: a person follows someone, the write settles, then they follow
/// someone else. That must keep working. `nmp-store`'s
/// `crash_atomicity_tests` cover the same fix at the `apply_plan` level; this
/// is the whole-engine story over a real relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_follows_after_full_settlement_do_not_refuse() {
    let author = fixed_keys();
    let first_target = Keys::generate();
    let second_target = Keys::generate();

    let indexer = ScriptedRelay::start(&RelayConfig::default()).await;
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    indexer
        .seed_relay_list(&author, &[relay.url.to_string()], &[], QUERY_CREATED_AT)
        .await;

    let engine = Arc::new(
        Engine::new_with_capabilities_and_routing(
            EngineConfig {
                app_relays: vec![relay.url.to_string()],
                ..EngineConfig::default()
            },
            vec![follow_capability()],
            Some(Box::new(nmp_outbox::Nip65Outbox::new([indexer
                .url
                .clone()]))),
        )
        .expect("direct follow engine must construct"),
    );
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("direct follow account must register");
    let writes = follow_writes();

    let first = set_following(
        &engine,
        &writes,
        first_target.public_key(),
        FollowChange::Follow,
    )
    .expect("first follow enters ordinary custody");
    let first_statuses = collect_follow_action(first, true);
    assert!(
        first_statuses
            .iter()
            .any(|status| matches!(status, NormFollowActionStatus::Receipt("settled"))),
        "the fixture must force full settlement before the second write, or this test proves \
         nothing about #1692: {first_statuses:?}"
    );

    let second = set_following(
        &engine,
        &writes,
        second_target.public_key(),
        FollowChange::Follow,
    )
    .expect("second follow enters ordinary custody -- #1692");
    let second_statuses = collect_follow_action(second, false);
    assert!(
        !second_statuses
            .iter()
            .any(|status| matches!(status, NormFollowActionStatus::Receipt("refused"))),
        "a sequential follow after full settlement must not be refused as stale -- #1692: \
         {second_statuses:?}"
    );

    engine.shutdown();
    indexer.shutdown();
    relay.shutdown();
}

/// A first follow accepted before ANY relay has answered survives a genuine
/// close/reopen, and the later real kind:3 source it eventually meets is
/// rebased under -- not discarded, and not unioned over.
///
/// The relay holds the contact-list query for 30s, so custody is entered with
/// no source truth at all: the capability default supplies one complete
/// pending kind:3. After the restart the relay returns with a NEWER
/// author-signed kind:3 carrying a `p` tag with a relay hint and a petname,
/// plus an `x` tag NIP-02 does not own. The successor must preserve both
/// verbatim and carry the followed target, and the ORIGINAL receipt must
/// still own it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_follow_survives_restart_and_replays_over_later_nip02_truth() {
    let delayed = RelayConfig {
        query_delay: Some(Duration::from_secs(30)),
        ..RelayConfig::default()
    };
    let mut relay = ScriptedRelay::start(&delayed).await;
    let relay_port = relay.port();
    let relay_url = relay.url.clone();
    let author = fixed_keys();
    let existing = Keys::generate().public_key();
    let target = Keys::generate().public_key();
    let directory = tempfile::tempdir().expect("persistent NIP-02 fixture directory");
    let store_path = directory.path().join("nip02-first-value-replay.redb");
    let config = || EngineConfig {
        store_path: Some(store_path.to_string_lossy().into_owned()),
        app_relays: vec![relay_url.to_string()],
        fallback_relays: vec![],
        ..EngineConfig::default()
    };

    let engine = Arc::new(
        Engine::new_with_capabilities_and_routing(
            config(),
            vec![follow_capability()],
            outbox_provider(),
        )
        .expect("persistent NIP-02 engine opens"),
    );
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("NIP-02 author registers");
    let observation = observe_following(Arc::clone(&engine), target)
        .expect("first-value relationship observation opens");
    assert!(
        relay
            .wait_query_for_kind(Kind::ContactList.as_u16(), WAIT)
            .await,
        "the delayed relay holds the contact-list request before first-value custody"
    );

    let writes = follow_writes();
    let action = set_following(&engine, &writes, target, FollowChange::Follow)
        .expect("first follow enters ordinary custody without relay-ready source truth");
    let receipt_id = action.id;
    let first = wait_for_relationship(&observation, FollowRelationship::Following);
    assert!(
        first.base_event_id.is_some(),
        "the capability default produces one complete pending kind:3"
    );
    assert_eq!(
        engine.publish_queue(None, u8::MAX).unwrap()[0].receipt_id,
        receipt_id,
        "the first generation is owned by the ordinary receipt"
    );

    engine.shutdown();
    drop(action);
    drop(observation);
    drop(engine);
    relay.disconnect().await;

    let reopened = Arc::new(
        Engine::new_with_capabilities_and_routing(
            config(),
            vec![follow_capability()],
            outbox_provider(),
        )
        .expect("persistent NIP-02 engine reopens"),
    );
    reopened
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("NIP-02 author reattaches");
    assert!(
        matches!(
            reopened
                .reattach_receipt(receipt_id)
                .expect("ordinary receipt reattaches after restart"),
            ReceiptReattachment::Attached { .. }
        ),
        "construction eagerly restores NIP-02 and the exact first-follow receipt"
    );

    relay = ScriptedRelay::start_on_port(relay_port, &RelayConfig::default()).await;
    assert_eq!(relay.url, relay_url);
    let preserved_contact = Tag::parse([
        "p".to_string(),
        existing.to_hex(),
        "wss://hint.example".to_string(),
        "petname".to_string(),
    ])
    .unwrap();
    let preserved_unrelated = Tag::parse([
        "x".to_string(),
        "remote-owned".to_string(),
        "extra".to_string(),
    ])
    .unwrap();
    let relay_source = EventBuilder::new(Kind::ContactList, "relay-owned content")
        .tags([preserved_contact.clone(), preserved_unrelated.clone()])
        .custom_created_at(Timestamp::from(
            Timestamp::now().as_secs().saturating_add(5),
        ))
        .sign_with_keys(&author)
        .expect("later NIP-02 source signs");
    relay.seed_signed_event(&relay_source).await;

    let reopened_observation = observe_following(Arc::clone(&reopened), target)
        .expect("reopened relationship observation requests later source truth");
    let _ = wait_for_relationship(&reopened_observation, FollowRelationship::Following);
    let deadline = Instant::now() + WAIT;
    let successor = loop {
        if let Some(event) = relay.admitted_events().into_iter().find(|event| {
            event.content == "relay-owned content"
                && event.tags.iter().any(|tag| tag == &preserved_contact)
                && event.tags.iter().any(|tag| tag == &preserved_unrelated)
                && event.tags.iter().any(|tag| {
                    let row = tag.as_slice();
                    row.first().is_some_and(|cell| cell == "p")
                        && row.get(1).is_some_and(|cell| cell == &target.to_hex())
                })
        }) {
            break event;
        }
        assert!(
            Instant::now() < deadline,
            "later relay truth never produced a tag-preserving NIP-02 successor; admitted={:?}",
            relay.admitted_events()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let queue = reopened.publish_queue(None, u8::MAX).unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].receipt_id, receipt_id);
    assert_eq!(queue[0].event_id, successor.id);

    reopened.shutdown();
    drop(reopened_observation);
    relay.shutdown();
}

fn wait_for_relationship(
    observation: &nmp_nip02::FollowObservation,
    expected: FollowRelationship,
) -> nmp_nip02::FollowSnapshot {
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let snapshot = observation
            .recv_timeout(remaining)
            .unwrap_or_else(|error| panic!("relationship never reached {expected:?}: {error:?}"));
        if snapshot.relationship == expected {
            return snapshot;
        }
    }
}

/// A follow accepted while the engine is OFFLINE recomputes the derived
/// follow feed immediately -- before any relay confirms anything -- and the
/// later source rebase REPLACES the cached row rather than unioning with it.
///
/// The account here holds only a public key: there is no signer, so nothing
/// can be signed or published. That is deliberate. It proves the visible
/// answer a query gives is produced by local custody and recomputation alone,
/// with publication removed from the picture entirely (the final assertion
/// checks the relay admitted nothing).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_follow_recomputes_derived_feed_before_later_source_rebase() {
    let mut relay = ScriptedRelay::start(&RelayConfig {
        hold_query_kind: Some(Kind::ContactList.as_u16()),
        ..RelayConfig::default()
    })
    .await;
    let relay_port = relay.port();
    let relay_url = relay.url.clone();
    let author = fixed_keys();
    let original = (0..5).map(|_| Keys::generate()).collect::<Vec<_>>();
    let target = Keys::generate();
    let remote_additions = (0..3).map(|_| Keys::generate()).collect::<Vec<_>>();
    let directory = tempfile::tempdir().expect("derived-follow fixture directory");
    let store_path = directory.path().join("nip02-derived-follow.redb");

    let cached_unrelated = Tag::parse(["x", "cached-owned"]).expect("unrelated cached tag parses");
    let cached_contact = EventBuilder::new(Kind::ContactList, "cached contact content")
        .tags(
            original
                .iter()
                .map(|keys| Tag::public_key(keys.public_key()))
                .chain(std::iter::once(cached_unrelated.clone())),
        )
        .custom_created_at(Timestamp::from(1_800_000_000))
        .sign_with_keys(&author)
        .expect("cached contact list signs");
    {
        let mut store = RedbStore::open(&store_path).expect("derived-follow store opens");
        store
            .insert(
                cached_contact,
                RelayObserved::new(relay_url.clone(), Timestamp::from(1_800_000_001)),
            )
            .expect("cached contact list is source-observed");
        for (index, keys) in original
            .iter()
            .chain(std::iter::once(&target))
            .chain(remote_additions.iter())
            .enumerate()
        {
            let note = EventBuilder::new(Kind::TextNote, format!("note-{index}"))
                .custom_created_at(Timestamp::from(1_800_000_100 + index as u64))
                .sign_with_keys(keys)
                .expect("cached note signs");
            store
                .insert(
                    note,
                    RelayObserved::new(
                        relay_url.clone(),
                        Timestamp::from(1_800_000_200 + index as u64),
                    ),
                )
                .expect("cached note is source-observed");
        }
    }

    let config = || EngineConfig {
        store_path: Some(store_path.to_string_lossy().into_owned()),
        app_relays: vec![relay_url.to_string()],
        fallback_relays: vec![],
        ..EngineConfig::default()
    };
    let engine = Engine::new_with_capabilities_and_routing(
        config(),
        vec![follow_capability()],
        outbox_provider(),
    )
    .expect("persistent NIP-02 engine opens");
    engine
        .add_public_key_account(author.public_key(), true)
        .expect("NIP-02 author registers without a signer");
    let contact_list = engine
        .observe(
            LiveQuery::single(pinned_contact_list(author.public_key(), relay_url.clone())),
            None,
        )
        .expect("contact-list observation opens");
    let mut contact_rows = BTreeMap::new();
    assert!(
        relay
            .wait_query_for_kind(Kind::ContactList.as_u16(), WAIT)
            .await,
        "the relay never received the held contact-list request"
    );
    let feed = engine
        .observe(
            pinned_follow_feed(author.public_key(), relay_url.clone()),
            None,
        )
        .expect("derived follow feed opens");
    let mut feed_rows = BTreeMap::new();
    let original_authors = original
        .iter()
        .map(|keys| keys.public_key().to_hex())
        .collect::<BTreeSet<_>>();
    wait_for_contact_list(
        &contact_list,
        &mut contact_rows,
        &original_authors,
        "cached contact content",
        cached_unrelated.as_slice(),
        "cached contact list",
    );
    wait_for_note_authors(
        &feed,
        &mut feed_rows,
        &original_authors,
        "cached derived feed",
    );
    relay.disconnect().await;

    let writes = follow_writes();
    let action = set_following(&engine, &writes, target.public_key(), FollowChange::Follow)
        .expect("offline follow enters ordinary custody");
    let receipt_id = action.id;
    let mut after_follow = original_authors.clone();
    after_follow.insert(target.public_key().to_hex());
    wait_for_contact_list(
        &contact_list,
        &mut contact_rows,
        &after_follow,
        "cached contact content",
        cached_unrelated.as_slice(),
        "pending-follow contact list",
    );
    wait_for_note_authors(
        &feed,
        &mut feed_rows,
        &after_follow,
        "pending-follow derived feed",
    );
    let initial_queue = engine.publish_queue(None, u8::MAX).unwrap();
    assert_eq!(initial_queue.len(), 1);
    assert_eq!(initial_queue[0].receipt_id, receipt_id);
    let initial_event_id = initial_queue[0].event_id;

    contact_list.cancel();
    feed.cancel();
    engine.shutdown();
    drop(action);
    drop(engine);

    let engine = Engine::new_with_capabilities_and_routing(
        config(),
        vec![follow_capability()],
        outbox_provider(),
    )
    .expect("persistent NIP-02 engine reopens");
    engine
        .add_public_key_account(author.public_key(), true)
        .expect("NIP-02 author reattaches without a signer");
    assert!(
        matches!(
            engine
                .reattach_receipt(receipt_id)
                .expect("offline-follow receipt reattaches after restart"),
            ReceiptReattachment::Attached { .. }
        ),
        "the ordinary receipt must survive the offline engine restart"
    );
    relay = ScriptedRelay::start_on_port(
        relay_port,
        &RelayConfig {
            hold_query_kind: Some(Kind::ContactList.as_u16()),
            ..RelayConfig::default()
        },
    )
    .await;
    assert_eq!(relay.url, relay_url);
    let contact_list = engine
        .observe(
            LiveQuery::single(pinned_contact_list(author.public_key(), relay_url.clone())),
            None,
        )
        .expect("reopened contact-list observation opens");
    let mut contact_rows = BTreeMap::new();
    let feed = engine
        .observe(
            pinned_follow_feed(author.public_key(), relay_url.clone()),
            None,
        )
        .expect("reopened derived follow feed opens");
    let mut feed_rows = BTreeMap::new();
    let preserved_unrelated =
        Tag::parse(["x", "relay-owned", "extra"]).expect("unrelated relay tag parses");
    let remote_source = EventBuilder::new(Kind::ContactList, "relay-owned content")
        .tags(
            original[..4]
                .iter()
                .chain(remote_additions.iter())
                .map(|keys| Tag::public_key(keys.public_key()))
                .chain(std::iter::once(preserved_unrelated.clone())),
        )
        .custom_created_at(Timestamp::from(1_800_000_010))
        .sign_with_keys(&author)
        .expect("later relay contact list signs");
    assert!(
        relay
            .wait_query_for_kind(Kind::ContactList.as_u16(), WAIT)
            .await,
        "the reopened engine never sent the finite contact-list request; connections={}, \
         contacts={}",
        relay.connection_count(),
        relay.contact_count()
    );
    relay.seed_signed_event(&remote_source).await;
    relay.release_queries();

    let expected_rebased = original[..4]
        .iter()
        .chain(std::iter::once(&target))
        .chain(remote_additions.iter())
        .map(|keys| keys.public_key().to_hex())
        .collect::<BTreeSet<_>>();
    let rebased_row_id = wait_for_contact_list(
        &contact_list,
        &mut contact_rows,
        &expected_rebased,
        "relay-owned content",
        preserved_unrelated.as_slice(),
        "source-rebased contact list",
    );
    assert!(
        contact_rows.values().all(|row| {
            row.kind().as_u16() != Kind::ContactList.as_u16()
                || !row
                    .tags()
                    .iter()
                    .any(|tag| tag.as_slice() == cached_unrelated.as_slice())
        }),
        "the rebased row must replace stale cached metadata rather than unioning it"
    );
    wait_for_note_authors(
        &feed,
        &mut feed_rows,
        &expected_rebased,
        "source-rebased derived feed",
    );

    let queue = engine.publish_queue(None, u8::MAX).unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].receipt_id, receipt_id);
    assert_eq!(
        queue[0].event_id.to_hex(),
        rebased_row_id,
        "the original receipt must own the exact successor visible to queries"
    );
    assert_ne!(
        queue[0].event_id, initial_event_id,
        "later source truth installs one new pending generation under the same receipt"
    );
    assert!(
        relay.admitted_events().is_empty(),
        "a public-key-only account proves query visibility before signing or publication"
    );

    contact_list.cancel();
    feed.cancel();
    engine.shutdown();
    drop(engine);
    relay.shutdown();
}
