//! #1033 Lane E: what a multi-relay group publication and a multi-relay
//! group listing actually do on the wire, against `nmp::nip29`'s new
//! [`RelayScope`]/[`Group`] shape.
//!
//! The single-host discovery door and `Group`'s single-host constructor are
//! gone (#1033). The surface under test here is:
//!
//! ```text
//! nip29::on([host_a, host_b])?.group("photographers")
//!     .publish(&engine, author, builder)          // -> Explicit(hosts)
//!     .publish_signed(&engine, event)              // -> Explicit(hosts), no mutation
//! nip29::on([host_a, host_b])?.groups_where(&predicate)  // -> one LiveQuery, one branch per host
//! ```
//!
//! Four claims, each proved end to end over real sockets against real
//! in-process relays rather than by inspecting a minted value:
//!
//!   1. a group read/publication routes to the WHOLE relay set as
//!      `Explicit`, never widened, and never to a discovered outbox
//!      relay -- `a_group_write_routes_explicitly_to_the_whole_scope_and_never_to_the_author_outbox`;
//!   2. exactly one signed `h` tag reaches the wire, no other context tag --
//!      folded into the same test, checked at every host;
//!   3. pre-signed bytes stay exact: the event a caller already signed is
//!      byte-identical to what every host receives --
//!      `publish_signed_delivers_the_callers_exact_pre_signed_bytes_to_every_host`;
//!   4. the wire-level consequence of per-host demand stamping: one host's
//!      OWN member-list evidence must never answer for another host's
//!      listing of the SAME group id --
//!      `a_groups_where_listing_never_lets_one_hosts_member_evidence_answer_for_anothers_group`.
//!
//! (4) deliberately does not re-check the mutation-level graph shape --
//! `crates/nmp/src/nip29/mod.rs`'s own
//! `scope_stamps_exact_hosts_on_every_nested_nip29_demand` already pins that
//! every nested `Demand` is stamped with the exact host. What THAT test
//! cannot show is that getting it wrong would actually corrupt an answer:
//! two relays serving the SAME group id with DIVERGENT member-list evidence,
//! resolved through one real engine and one real `groups_where` query, prove
//! the cross-host leak this design exists to prevent is not just absent from
//! the graph shape but absent from the delivered rows.
//!
//! Prior art: PR #1163 (#1105) proved a single-host group write's exact
//! wire shape against a real `Engine` and three in-process relays, with the
//! author's outbox a relay the engine discovered for itself from a kind:10002
//! it fetched from an indexer -- so "untouched" was a claim about a
//! demonstrably live destination, not a relay that was never going to be
//! contacted. This file keeps that rigour and widens it to two group hosts.
//!
//! Same version-shadowing precaution as `runtime_integration.rs`: never
//! `use nostr_relay_builder::prelude::*` -- `nmp-test-support` owns the
//! bridge between the two pinned `nostr` versions.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use nmp::mechanism::runtime::FifoReceiver;
use nmp::nip29::{self, member_list_includes, Group, GroupContextError, GroupPublishError};
use nmp::{
    Binding, Engine, EngineConfig, EventBuilder, Filter, Row, RowDelta, SignerError, SignerOp,
    SignerPublicKey, SignerSignedEvent, SignerUnsignedEvent, SigningCapability, WriteStatus,
};
use nmp_local_signer::LocalKeySigner;
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::{EventId, JsonUtil, Keys, Kind, RelayUrl, Tag, Timestamp, UnsignedEvent};

const GROUP_ID: &str = "photographers";
const GROUP_KIND: u16 = 9;
const GROUP_METADATA_KIND: u16 = 39000;
const GROUP_MEMBERS_KIND: u16 = 39002;

/// Long enough for a real discovery/publish round trip on a loaded CI
/// runner, short enough that a genuine failure reports rather than hangs.
const SETTLE: Duration = Duration::from_secs(20);

fn signer(keys: &Keys) -> LocalKeySigner {
    LocalKeySigner::from_secret_bytes(keys.secret_key().as_secret_bytes())
        .expect("fixture keys are valid secp256k1 scalars")
}

/// A live engine with ONE operator input: where to look for relay lists.
/// Everything else about routing is discovered or minted.
fn engine_reading_lists_from(indexer: &ScriptedRelay, keys: &Keys) -> Engine {
    let engine = Engine::new(EngineConfig {
        indexer_relays: vec![indexer.url.to_string()],
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        ..EngineConfig::default()
    })
    .expect("an in-memory engine builds");
    let registration = engine
        .add_account(&keys.secret_key().to_secret_hex())
        .expect("the account registers");
    engine
        .set_active_account(Some(registration.public_key()))
        .expect("the account activates");
    engine
        .add_signer(signer(keys))
        .expect("a local signer registers");
    engine
}

/// A bare engine with no indexer at all -- the shape a purely `Explicit`
/// (`publish_signed`) or purely `Pinned` (`groups_where`) call needs, and
/// nothing more: neither operation ever consults a directory.
fn bare_engine() -> Engine {
    Engine::new(EngineConfig {
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        ..EngineConfig::default()
    })
    .expect("an in-memory engine builds")
}

/// A live engine with a real local signer for `keys`, and no indexer/outbox
/// wiring at all -- for capstones that only care about the group's own
/// explicit route, never about `Auto`/outbox behavior.
fn engine_with_signer(keys: &Keys) -> Engine {
    let engine = Engine::new(EngineConfig {
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        ..EngineConfig::default()
    })
    .expect("an in-memory engine builds");
    let registration = engine
        .add_account(&keys.secret_key().to_secret_hex())
        .expect("the account registers");
    engine
        .set_active_account(Some(registration.public_key()))
        .expect("the account activates");
    engine
        .add_signer(signer(keys))
        .expect("a local signer registers");
    engine
}

/// A signer that always refuses -- PROTOCOL-CONTEXTTAGISSIGNED-003's fixture.
/// A REFUSAL (`SignerError::Rejected`), not a transport outage: the write
/// reaches a terminal `Failed` fact rather than parking.
struct FailingSigner {
    pubkey: SignerPublicKey,
}

impl SigningCapability for FailingSigner {
    fn public_key(&self) -> Option<SignerPublicKey> {
        Some(self.pubkey)
    }

    fn sign(&self, _unsigned: SignerUnsignedEvent) -> SignerOp<SignerSignedEvent> {
        SignerOp::err(SignerError::Rejected(
            "test: signing is configured to fail".to_string(),
        ))
    }
}

/// A live engine whose one registered signer always refuses.
fn engine_with_failing_signer(keys: &Keys) -> Engine {
    let engine = Engine::new(EngineConfig {
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        ..EngineConfig::default()
    })
    .expect("an in-memory engine builds");
    engine
        .set_active_account(Some(keys.public_key()))
        .expect("the account activates");
    engine
        .add_signer(FailingSigner {
            pubkey: SignerPublicKey::new(keys.public_key().to_bytes()),
        })
        .expect("a failing signer still registers cleanly");
    engine
}

/// One group scoped to a single host -- the shape every new capstone below
/// needs; multi-host scoping is already covered by the tests above.
fn group(host: &RelayUrl) -> Group {
    nip29::on([host.clone()])
        .expect("one host forms a scope")
        .group(GROUP_ID)
}

/// Drain a receipt stream until `pred` holds, returning every status seen.
/// Bounded, and it reports what it DID see when it gives up, because "the
/// write never reached the host" and "the write was refused at the door" are
/// different failures.
fn drain_until(
    receipts: &FifoReceiver<WriteStatus>,
    pred: impl Fn(&WriteStatus) -> bool,
) -> Vec<WriteStatus> {
    let deadline = Instant::now() + SETTLE;
    let mut seen = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("receipt stream never satisfied the predicate; saw {seen:?}");
        }
        match receipts.recv_timeout(remaining) {
            Ok(status) => {
                let done = pred(&status);
                seen.push(status);
                if done {
                    return seen;
                }
            }
            Err(error) => panic!("receipt stream ended early ({error:?}); saw {seen:?}"),
        }
    }
}

/// Drain the receipt stream until EVERY relay in `expected` has produced an
/// `Acked` status at least once, returning every status seen. Unlike
/// `drain_until`, the predicate is cumulative across the whole drained
/// sequence rather than true-on-a-single-status, because a multi-host write
/// acks each host independently and possibly out of order.
fn drain_until_all_acked(
    receipts: &FifoReceiver<WriteStatus>,
    expected: &BTreeSet<RelayUrl>,
) -> Vec<WriteStatus> {
    let deadline = Instant::now() + SETTLE;
    let mut seen = Vec::new();
    let mut acked: BTreeSet<RelayUrl> = BTreeSet::new();
    loop {
        if &acked == expected {
            return seen;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("not every expected host acked; acked {acked:?} of {expected:?}; saw {seen:?}");
        }
        match receipts.recv_timeout(remaining) {
            Ok(status) => {
                if let WriteStatus::Acked(relay) = &status {
                    acked.insert(relay.clone());
                }
                seen.push(status);
            }
            Err(error) => panic!(
                "receipt stream ended early ({error:?}); acked {acked:?} of {expected:?}; \
                 saw {seen:?}"
            ),
        }
    }
}

fn wait_for_events(relay: &ScriptedRelay, count: usize) -> Vec<nostr::Event> {
    let deadline = Instant::now() + SETTLE;
    loop {
        let admitted = relay.admitted_events();
        if admitted.len() >= count {
            return admitted;
        }
        assert!(
            Instant::now() < deadline,
            "relay {} admitted {} of {count} expected events",
            relay.url,
            admitted.len()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn h_rows(event: &nostr::Event) -> Vec<String> {
    event
        .tags
        .iter()
        .map(|tag| tag.clone().to_vec())
        .filter(|row| row.first().map(String::as_str) == Some("h"))
        .map(|row| row.get(1).cloned().unwrap_or_default())
        .collect()
}

fn relays_named_by(statuses: &[WriteStatus]) -> BTreeSet<RelayUrl> {
    let mut named = BTreeSet::new();
    for status in statuses {
        match status {
            WriteStatus::Routed { relays, .. } => named.extend(relays.iter().cloned()),
            WriteStatus::Sent { relay, .. }
            | WriteStatus::Acked(relay)
            | WriteStatus::Rejected(relay, _)
            | WriteStatus::GaveUp(relay)
            | WriteStatus::AwaitingRelay { relay }
            | WriteStatus::AwaitingAuth { relay }
            | WriteStatus::AuthDenied { relay, .. }
            | WriteStatus::RetryEligible { relay, .. }
            | WriteStatus::HandoffAmbiguous { relay, .. }
            | WriteStatus::PersistenceBlocked(relay)
            | WriteStatus::RoutePersistenceBlocked(relay)
            | WriteStatus::OutcomeUnknown(relay) => {
                named.insert(relay.clone());
            }
            _ => {}
        }
    }
    named
}

/// One relay-signed NIP-29 fixture event: `kind` at `created_at`, carrying
/// `tags`, signed by `signer`. Used to stand in for a relay's OWN
/// kind:39000/39002 records -- which relay signed it is irrelevant to these
/// tests; only which HOST serves it matters.
fn relay_signed_event(signer: &Keys, kind: u16, created_at: u64, tags: Vec<Tag>) -> nostr::Event {
    UnsignedEvent::new(
        signer.public_key(),
        Timestamp::from(created_at),
        Kind::from(kind),
        tags,
        String::new(),
    )
    .sign_with_keys(signer)
    .expect("fixture keys sign cleanly")
}

fn tag2(name: &str, value: &str) -> Tag {
    Tag::parse([name, value]).expect("a two-value row is well-formed")
}

/// The accumulated row-set projection an app builds from a raw delta stream
/// -- same discipline as `runtime_integration.rs`'s `wait_for_rows`, kept at
/// the `Row` (event + sources) level because falsifier 4 needs `sources`,
/// not just the event.
fn wait_for_group_rows(
    subscription: &nmp::Subscription,
    timeout: Duration,
    pred: impl Fn(&BTreeMap<EventId, Row>) -> bool,
) -> BTreeMap<EventId, Row> {
    let deadline = Instant::now() + timeout;
    let mut current: BTreeMap<EventId, Row> = BTreeMap::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "group rows never satisfied the predicate; saw {:?}",
                current.keys().collect::<Vec<_>>()
            );
        }
        match subscription.recv_timeout(remaining) {
            Ok(frame) => {
                apply(&mut current, frame.deltas);
                if pred(&current) {
                    return current;
                }
            }
            Err(error) => panic!(
                "subscription ended before the predicate was satisfied ({error:?}); saw {:?}",
                current.keys().collect::<Vec<_>>()
            ),
        }
    }
}

fn apply(current: &mut BTreeMap<EventId, Row>, deltas: Vec<RowDelta>) {
    for delta in deltas {
        match delta {
            RowDelta::Added(row) => {
                current.insert(row.event.id, row);
            }
            RowDelta::SourcesGrew { id, sources } => {
                if let Some(row) = current.get_mut(&id) {
                    row.sources = sources;
                }
            }
            RowDelta::Removed(id) => {
                current.remove(&id);
            }
        }
    }
}

/// After `pred` first holds, keep draining for a bounded quiet window so a
/// LATE, WRONG row (the exact shape a cross-host leak would produce) has a
/// real chance to arrive before the negative assertion runs. Never blocks
/// past `quiet` once nothing new shows up.
fn settle_group_rows(
    subscription: &nmp::Subscription,
    mut current: BTreeMap<EventId, Row>,
    quiet: Duration,
) -> BTreeMap<EventId, Row> {
    while let Ok(frame) = subscription.recv_timeout(quiet) {
        apply(&mut current, frame.deltas);
    }
    current
}

/// THE proof for falsifiers 1 and 2. One process, one engine, an indexer, an
/// author outbox the engine DISCOVERS for itself, and TWO independent group
/// hosts. A group write must reach the WHOLE host set as `Explicit` -- never
/// narrowed to one, never widened with the outbox, never `Auto` -- and must
/// carry exactly one `h` row and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_write_routes_explicitly_to_the_whole_scope_and_never_to_the_author_outbox() {
    let keys = Keys::generate();

    let indexer = ScriptedRelay::start(&RelayConfig::default()).await;
    let outbox = ScriptedRelay::start(&RelayConfig::default()).await;
    let host_a = ScriptedRelay::start(&RelayConfig::default()).await;
    let host_b = ScriptedRelay::start(&RelayConfig::default()).await;
    // The author's real relay list, published where the engine's own
    // discovery subscription looks. Nothing is injected into the router.
    indexer
        .seed_relay_list(&keys, &[outbox.url.to_string()], &[], 1_700_000_000)
        .await;

    let engine = engine_reading_lists_from(&indexer, &keys);

    // ---- leg 1: the author outbox is real ---------------------------------
    let auto_receipts = engine
        .publish(nmp::WriteIntent {
            payload: nmp::WritePayload::Event(
                EventBuilder::new(Kind::TextNote).content("ordinary"),
            ),
            durability: nmp::Durability::Durable,
            routing: nmp::WriteRouting::Auto,
            identity: nmp::Identity::Active,
            correlation: None,
        })
        .expect("an Auto publish is accepted");
    let auto_statuses = drain_until(&auto_receipts, |status| {
        matches!(status, WriteStatus::Acked(_))
    });
    assert_eq!(
        relays_named_by(&auto_statuses),
        BTreeSet::from([outbox.url.clone()]),
        "Auto resolved to the author's own discovered write relay, and only that"
    );
    let ordinary = wait_for_events(&outbox, 1);
    assert_eq!(ordinary.len(), 1, "exactly the one ordinary note");

    // ---- leg 2: the group write, over a TWO-host scope ---------------------
    let contacts_before = outbox.contact_count();
    let group = nip29::on([host_a.url.clone(), host_b.url.clone()])
        .expect("two hosts form a scope")
        .group(GROUP_ID);
    // Content only. No relay, no routing value, no tag: the door takes none
    // of them, so this call cannot express them.
    let receipts = group
        .publish(
            &engine,
            keys.public_key(),
            EventBuilder::new(Kind::from(GROUP_KIND)).content("first light"),
        )
        .expect("the group publication is accepted");
    let expected_hosts = BTreeSet::from([host_a.url.clone(), host_b.url.clone()]);
    let statuses = drain_until_all_acked(&receipts, &expected_hosts);

    let delivered_a = wait_for_events(&host_a, 1);
    let delivered_b = wait_for_events(&host_b, 1);
    for (host_name, delivered) in [("A", &delivered_a), ("B", &delivered_b)] {
        assert_eq!(
            delivered.len(),
            1,
            "host {host_name} received exactly one event"
        );
        let event = &delivered[0];
        assert_eq!(event.content, "first light");
        assert_eq!(
            h_rows(event),
            vec![GROUP_ID.to_string()],
            "host {host_name}: the group appended exactly one h row, carrying the group id"
        );
        assert_eq!(
            event.tags.len(),
            1,
            "host {host_name}: the group contributed the h row and nothing else: {:?}",
            event.tags
        );
        assert!(
            event.verify().is_ok(),
            "host {host_name}: the id and signature cover the bytes the h row is inside"
        );
    }
    assert_eq!(
        delivered_a[0].id, delivered_b[0].id,
        "the SAME signed event reached both hosts -- one signature, one h row, one route"
    );

    // Every relay fact this write produced names ONLY the scope's two hosts
    // -- never one alone (narrowed), never a third (widened). Keep draining
    // briefly past the "every host acked" point so a stray fact naming a
    // relay outside the scope (the exact shape a widened route would
    // produce) has a real chance to arrive before this assertion runs.
    let mut every_named = relays_named_by(&statuses);
    while let Ok(status) = receipts.recv_timeout(Duration::from_millis(300)) {
        every_named.extend(relays_named_by(std::slice::from_ref(&status)));
    }
    assert_eq!(
        every_named, expected_hosts,
        "every relay fact this write produced names exactly the scope's two hosts"
    );

    // The author outbox: known, healthy, and irrelevant to a group write.
    assert_eq!(
        outbox.admitted_events().len(),
        1,
        "the author's write relay received the ordinary note and NOT the group event"
    );
    assert_eq!(
        outbox.contact_count(),
        contacts_before,
        "the author's write relay was not contacted at all for the group write"
    );

    engine.shutdown();
    host_a.shutdown();
    host_b.shutdown();
    outbox.shutdown();
    indexer.shutdown();
}

// ===========================================================================
// PROTOCOL-APPSUPPLIEDCONTEXTREFUSED-006 -- a caller-supplied-context refusal
// is a LOCAL typed refusal, decided before the relay is ever contacted, and
// it is observably different from an ordinary relay rejection of the same
// kind of write.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_caller_supplied_context_is_refused_before_relay_contact_and_differs_from_a_relay_rejection(
) {
    let keys = Keys::generate();
    let host = ScriptedRelay::start(&RelayConfig {
        reject_writes: Some("blocked: ordinary relay rejection".to_string()),
        ..RelayConfig::default()
    })
    .await;
    let engine = engine_with_signer(&keys);
    let group = group(&host.url);

    // Half 1: a caller-supplied h is refused LOCALLY -- the relay, which
    // rejects everything, is never even contacted for it.
    let refused = group.publish(
        &engine,
        keys.public_key(),
        EventBuilder::new(Kind::from(GROUP_KIND)).tag(Tag::parse(["h", GROUP_ID]).unwrap()),
    );
    let refused_error = refused.err().expect("a caller-supplied h must be refused");
    assert!(
        matches!(
            refused_error,
            GroupPublishError::Context(GroupContextError::CallerSuppliedContext)
        ),
        "expected a local Context refusal, got {refused_error}"
    );
    assert_eq!(
        host.contact_count(),
        0,
        "a caller-supplied-context refusal must never reach the relay"
    );
    assert!(host.wire_record().event_ids.is_empty());

    // Half 2: an ordinary write to the SAME relay reaches it and is refused
    // BY THE RELAY -- a completely different, Engine-side error shape, only
    // observable after the relay was actually contacted.
    let receipts = group
        .publish(
            &engine,
            keys.public_key(),
            EventBuilder::new(Kind::from(GROUP_KIND)).content("first light"),
        )
        .expect("an ordinary draft is accepted at the door");
    let statuses = drain_until(&receipts, |status| {
        matches!(status, WriteStatus::Rejected(_, _))
    });
    assert!(
        statuses.iter().any(
            |status| matches!(status, WriteStatus::Rejected(relay, message)
                if relay == &host.url && message.contains("ordinary relay rejection"))
        ),
        "expected the relay's own rejection message on the receipt: {statuses:?}"
    );
    assert!(
        host.contact_count() > 0,
        "the ordinary write must have reached the relay"
    );

    engine.shutdown();
    host.shutdown();
}

// ===========================================================================
// PROTOCOL-CONTEXTTAGISSIGNED-003 -- a signing failure leaves nothing on the
// wire: no EVENT frame ever reaches the relay, and no receipt implies
// delivery.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signing_failure_leaves_no_event_frame_and_no_delivery_implying_receipt() {
    let keys = Keys::generate();
    let host = ScriptedRelay::start(&RelayConfig::default()).await;
    let engine = engine_with_failing_signer(&keys);
    let group = group(&host.url);

    let receipts = group
        .publish(
            &engine,
            keys.public_key(),
            EventBuilder::new(Kind::from(GROUP_KIND)).content("first light"),
        )
        .expect("the door accepts the draft; the signer fails afterward");
    let statuses = drain_until(
        &receipts,
        |status| matches!(status, WriteStatus::Failed(reason) if reason.to_lowercase().contains("sign")),
    );

    assert!(
        !statuses.iter().any(|status| matches!(
            status,
            WriteStatus::Sent { .. }
                | WriteStatus::Acked(_)
                | WriteStatus::Rejected(_, _)
                | WriteStatus::HandoffAmbiguous { .. }
        )),
        "no fact implying relay delivery may appear on a signing failure: {statuses:?}"
    );
    assert!(
        host.wire_record().event_ids.is_empty(),
        "no EVENT frame may ever reach the relay when signing fails"
    );
    assert_eq!(
        host.contact_count(),
        0,
        "a signing failure never contacts the relay at all"
    );

    engine.shutdown();
    host.shutdown();
}

/// Falsifier 3: pre-signed bytes stay exact. The caller signs the event
/// itself (with its own correct `h` row already in place) and hands the
/// EXACT `Event` to `publish_signed`. What reaches every host in the scope
/// must be byte-identical to what the caller signed -- same id, same
/// signature, same canonical JSON -- because `publish_signed` VALIDATES the
/// context row rather than appending one, and appending (or re-signing)
/// would change the bytes and therefore the `EventId` the caller already
/// holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_signed_delivers_the_callers_exact_pre_signed_bytes_to_every_host() {
    let author = Keys::generate();
    let host_a = ScriptedRelay::start(&RelayConfig::default()).await;
    let host_b = ScriptedRelay::start(&RelayConfig::default()).await;
    let engine = bare_engine();

    let signed = UnsignedEvent::new(
        author.public_key(),
        Timestamp::from(1_700_000_000u64),
        Kind::from(GROUP_KIND),
        vec![tag2("h", GROUP_ID)],
        "already signed, byte for byte".to_string(),
    )
    .sign_with_keys(&author)
    .expect("fixture keys sign cleanly");
    let sent_json = signed.as_json();

    let group = nip29::on([host_a.url.clone(), host_b.url.clone()])
        .expect("two hosts form a scope")
        .group(GROUP_ID);
    let receipts = group
        .publish_signed(&engine, signed.clone())
        .expect("a correctly-contextualized signed event is accepted");
    let statuses = drain_until(
        &receipts,
        |status| matches!(status, WriteStatus::Acked(relay) if *relay == host_b.url),
    );
    assert_eq!(
        relays_named_by(&statuses),
        BTreeSet::from([host_a.url.clone(), host_b.url.clone()]),
        "a pre-signed group write routes Explicit to every host in the scope, same as a draft"
    );

    for (host_name, host) in [("A", &host_a), ("B", &host_b)] {
        let delivered = wait_for_events(host, 1);
        assert_eq!(
            delivered.len(),
            1,
            "host {host_name} received exactly one event"
        );
        let received = &delivered[0];
        assert_eq!(
            received.id, signed.id,
            "host {host_name}: the event id the caller already computed must be unchanged"
        );
        assert_eq!(
            received.sig, signed.sig,
            "host {host_name}: the signature the caller already produced must be unchanged"
        );
        assert_eq!(
            received.as_json(),
            sent_json,
            "host {host_name}: the canonical bytes that crossed the wire must be byte-identical \
             to what the caller signed -- no re-append, no re-sign, no reorder"
        );
    }

    engine.shutdown();
    host_a.shutdown();
    host_b.shutdown();
}

// ===========================================================================
// PROTOCOL-PRESIGNEDPUBLICATION-002 -- the id is known BEFORE publication, so
// an app can arm a live query on the event's own real, dynamically computed
// id (never a fixed fixture label) before the write ever goes out.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pre_signed_event_s_own_id_is_known_before_publication_and_matches_a_live_query_armed_on_it(
) {
    let me = Keys::generate();
    let host = ScriptedRelay::start(&RelayConfig::default()).await;
    let engine = bare_engine();
    let group = group(&host.url);

    let pre_signed = UnsignedEvent::new(
        me.public_key(),
        Timestamp::from(1_700_000_001u64),
        Kind::from(GROUP_KIND),
        vec![Tag::parse(["h", GROUP_ID]).unwrap()],
        "arm on my own id before I publish".to_string(),
    )
    .sign_with_keys(&me)
    .expect("fixture keys sign cleanly");
    // The id is a real, runtime-computed fact -- never a fixed hex fixture
    // label -- and it is known to the app BEFORE the write is issued.
    let known_id = pre_signed.id;

    let query = group
        .read(Filter {
            ids: Some(Binding::Literal(BTreeSet::from([known_id.to_hex()]))),
            ..Filter::default()
        })
        .expect("a live query armed on the exact id opens");
    let subscription = engine
        .observe(query, None)
        .expect("a live query armed on the exact id opens");

    let receipts = group
        .publish_signed(&engine, pre_signed)
        .expect("a correctly contextualized signed event is accepted");
    drain_until(&receipts, |status| matches!(status, WriteStatus::Acked(_)));

    let deadline = Instant::now() + SETTLE;
    let mut matched = false;
    while Instant::now() < deadline && !matched {
        let frame = subscription
            .recv_timeout(SETTLE)
            .expect("receive a projection from the armed query");
        matched = frame
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == known_id));
    }
    assert!(
        matched,
        "the query armed on the event's own pre-known id must match the event that reached the host"
    );

    drop(subscription);
    engine.shutdown();
    host.shutdown();
}

// ===========================================================================
// PROTOCOL-PRESIGNEDPUBLICATION-006 (wire witness) -- the route follows the
// group, never the signature: a pre-signed event authored by SOMEONE ELSE
// still lands only at the group's host, and that author's own real,
// discoverable write relay is never contacted.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pre_signed_event_from_another_author_routes_only_to_the_host_never_to_their_own_outbox()
{
    let me = Keys::generate();
    let author = Keys::generate();

    let indexer = ScriptedRelay::start(&RelayConfig::default()).await;
    let author_outbox = ScriptedRelay::start(&RelayConfig::default()).await;
    let host = ScriptedRelay::start(&RelayConfig::default()).await;
    indexer
        .seed_relay_list(
            &author,
            &[author_outbox.url.to_string()],
            &[],
            1_700_000_000,
        )
        .await;

    // The engine has a REAL, indexer-discoverable route for the author's own
    // outbox -- it would find it if it ever looked. `publish_signed` never
    // looks, because a group write is always `Explicit`.
    let engine = engine_reading_lists_from(&indexer, &me);
    let group = group(&host.url);

    let pre_signed = UnsignedEvent::new(
        author.public_key(),
        Timestamp::from(1_700_000_002u64),
        Kind::from(GROUP_KIND),
        vec![Tag::parse(["h", GROUP_ID]).unwrap()],
        "not mine".to_string(),
    )
    .sign_with_keys(&author)
    .expect("fixture keys sign cleanly");

    let receipts = group
        .publish_signed(&engine, pre_signed.clone())
        .expect("a correctly contextualized signed event from another author is accepted");
    let statuses = drain_until(&receipts, |status| matches!(status, WriteStatus::Acked(_)));

    assert_eq!(
        relays_named_by(&statuses),
        BTreeSet::from([host.url.clone()]),
        "every relay fact this write produced names the host and nothing else"
    );
    let delivered = wait_for_events(&host, 1);
    assert_eq!(delivered[0], pre_signed);
    assert_eq!(
        delivered[0].pubkey,
        author.public_key(),
        "the signature still belongs to the original author, not to me"
    );
    assert!(
        author_outbox.admitted_events().is_empty(),
        "the author's own outbox must never receive a group write published on their behalf"
    );
    assert_eq!(
        author_outbox.contact_count(),
        0,
        "the author's own outbox must never even be CONTACTED for a group write"
    );

    engine.shutdown();
    host.shutdown();
    author_outbox.shutdown();
    indexer.shutdown();
}

// ===========================================================================
// PROTOCOL-PRESIGNEDPUBLICATION-007 -- a host rejection of a pre-signed
// event is an ORDINARY receipt, tied to the same pre-known id: no re-sign,
// no re-route.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_rejection_of_a_pre_signed_event_is_an_ordinary_receipt_tied_to_its_unchanged_known_id(
) {
    let me = Keys::generate();
    let host = ScriptedRelay::start(&RelayConfig {
        reject_writes: Some("blocked: host refuses every event".to_string()),
        ..RelayConfig::default()
    })
    .await;
    let engine = bare_engine();
    let group = group(&host.url);

    let pre_signed = UnsignedEvent::new(
        me.public_key(),
        Timestamp::from(1_700_000_003u64),
        Kind::from(GROUP_KIND),
        vec![Tag::parse(["h", GROUP_ID]).unwrap()],
        "first light".to_string(),
    )
    .sign_with_keys(&me)
    .expect("fixture keys sign cleanly");
    let known_id = pre_signed.id;

    let receipts = group
        .publish_signed(&engine, pre_signed)
        .expect("a correctly contextualized signed event is accepted");
    let statuses = drain_until(&receipts, |status| {
        matches!(status, WriteStatus::Rejected(_, _))
    });

    assert!(
        statuses.iter().any(|status| matches!(
            status,
            WriteStatus::Rejected(relay, message)
                if relay == &host.url && message.contains("host refuses every event")
        )),
        "the receipt must carry an ordinary per-relay Rejected fact: {statuses:?}"
    );
    // `WriteStatus::Signed` is an ordinary lifecycle beat the engine emits
    // for an already-signed payload too, so the falsifiable claim here is
    // narrower and stronger: IF it appears, it must name the exact id the
    // caller already had, never a freshly minted one.
    assert!(
        statuses
            .iter()
            .all(|status| !matches!(status, WriteStatus::Signed(id) if *id != known_id)),
        "a pre-signed event must never be re-signed into a different id: {statuses:?}"
    );
    assert_eq!(
        relays_named_by(&statuses),
        BTreeSet::from([host.url.clone()]),
        "the rejected write must never have been re-routed anywhere else"
    );
    let attempted = host.wire_record().event_ids;
    assert_eq!(
        attempted,
        vec![known_id.to_hex()],
        "the id attempted on the wire is the exact id the caller already knew"
    );

    engine.shutdown();
    host.shutdown();
}

/// Falsifier 4: the wire-level consequence of per-host demand stamping.
/// Two hosts both serve a group named `photographers`, but NIP-29 authority
/// is per-relay -- these are two independent groups that happen to share a
/// name. Host A's own kind:39002 evidence names `member`; host B's does not.
/// A `groups_where(member_list_includes(member))` listing over BOTH hosts
/// must surface host A's `photographers` and must NEVER let host A's
/// evidence answer for host B's -- host B's row must not appear, because
/// nothing observed AT host B supports it.
///
/// This test found a real defect when it landed, and the defect is fixed.
/// `pinned_public_at` used to build every `Pinned`-sourced NIP-29 demand (the
/// outer listing AND every nested evidence lookup) leaving
/// `nmp_grammar::Demand`'s default `CacheMode::Agnostic` -- "serve every
/// matching cached row regardless of provenance".
/// `SourceAuthority::Pinned` alone scopes only the WIRE request; which locally
/// cached rows may ANSWER is governed independently by `CacheMode`. So once
/// host A's own kind:39002 event landed in the shared store, host B's
/// structurally-identical inner evidence lookup (same kind, same `#p`,
/// different `Pinned` host) resolved against that SAME cached row and answered
/// non-empty for host B -- the cross-host leak this design exists to prevent,
/// at the cache layer rather than the graph-shape layer.
///
/// `pinned_public_at` now sets `cache = CacheMode::Strict` at every level it
/// builds, so a cached row answers a branch only when its own provenance names
/// that branch's host. Host B's outer branch resolves zero atoms and never
/// sends its `#d` REQ.
///
/// This is not a re-check of `scope_stamps_exact_hosts_on_every_nested_nip29_demand`
/// (which pins the graph SHAPE): it proves that if the shape regressed --
/// e.g. every branch's inner member-evidence query silently fell back to one
/// host -- an app would receive a confidently WRONG row set, not a merely
/// malformed `Demand`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_groups_where_listing_never_lets_one_hosts_member_evidence_answer_for_anothers_group() {
    let host_a_signer = Keys::generate(); // stands in for host A's own relay-signed records
    let host_b_signer = Keys::generate(); // stands in for host B's own relay-signed records
    let member = Keys::generate().public_key(); // the subject the predicate asks about

    let host_a = ScriptedRelay::start(&RelayConfig::default()).await;
    let host_b = ScriptedRelay::start(&RelayConfig::default()).await;

    // Host A: "photographers" exists, and host A's OWN member-list evidence
    // names `member`.
    let host_a_metadata = relay_signed_event(
        &host_a_signer,
        GROUP_METADATA_KIND,
        1_700_000_000,
        vec![tag2("d", GROUP_ID)],
    );
    host_a.seed_signed_event(&host_a_metadata).await;
    host_a
        .seed_signed_event(&relay_signed_event(
            &host_a_signer,
            GROUP_MEMBERS_KIND,
            1_700_000_000,
            vec![tag2("d", GROUP_ID), tag2("p", &member.to_hex())],
        ))
        .await;

    // Host B: the SAME group id exists (a DIFFERENT relay's own record, a
    // different event entirely), but host B's own member-list evidence never
    // names `member` -- there is none at all, which is absence, not
    // negative evidence, and must not borrow host A's.
    let host_b_metadata = relay_signed_event(
        &host_b_signer,
        GROUP_METADATA_KIND,
        1_700_000_000,
        vec![tag2("d", GROUP_ID)],
    );
    host_b.seed_signed_event(&host_b_metadata).await;
    assert_ne!(
        host_a_metadata.id, host_b_metadata.id,
        "two relays' own kind:39000 records for the same group id are two distinct events"
    );

    let engine = bare_engine();
    let scope =
        nip29::on([host_a.url.clone(), host_b.url.clone()]).expect("two hosts form a scope");
    let predicate = member_list_includes(Binding::Literal(BTreeSet::from([member.to_hex()])));
    let query = scope
        .groups_where(&predicate)
        .expect("a two-host listing declares two branches");
    let subscription = engine
        .observe(query, None)
        .expect("a NIP-29 listing is an ordinary live query");

    let rows = wait_for_group_rows(&subscription, SETTLE, |rows| {
        rows.contains_key(&host_a_metadata.id)
    });
    // Give a late, WRONG row (the exact shape a cross-host leak produces) a
    // real chance to arrive before the negative assertion runs.
    let rows = settle_group_rows(&subscription, rows, Duration::from_millis(500));

    assert!(
        rows.contains_key(&host_a_metadata.id),
        "host A's own group must surface: its own evidence supports it. saw {:?}",
        rows.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        rows[&host_a_metadata.id].sources,
        BTreeSet::from([host_a.url.clone()]),
        "host A's row must be sourced from host A alone"
    );
    assert!(
        !rows.contains_key(&host_b_metadata.id),
        "host B's group must NOT surface: host A's member evidence must never answer for \
         host B's listing of the same group id. saw {:?}",
        rows.keys().collect::<Vec<_>>()
    );

    engine.shutdown();
    host_a.shutdown();
    host_b.shutdown();
}

// ===========================================================================
// PROTOCOL-PUBLISHINGTHROUGHTHEGROUP-005 -- two DIFFERENT groups, each on its
// own host, never bleed into each other: publishing into one never reaches
// the other's host, and each host receives only the event carrying its own
// group's h.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_groups_on_two_hosts_never_bleed_into_each_other_at_the_wire() {
    let keys = Keys::generate();
    let photographers_host = ScriptedRelay::start(&RelayConfig::default()).await;
    let darkroom_host = ScriptedRelay::start(&RelayConfig::default()).await;
    let engine = engine_with_signer(&keys);

    let photographers = nip29::on([photographers_host.url.clone()])
        .expect("one host forms a scope")
        .group("photographers");
    let darkroom = nip29::on([darkroom_host.url.clone()])
        .expect("one host forms a scope")
        .group("darkroom");

    let photographers_receipts = photographers
        .publish(
            &engine,
            keys.public_key(),
            EventBuilder::new(Kind::from(GROUP_KIND)).content("first light"),
        )
        .expect("the photographers publication is accepted");
    let darkroom_receipts = darkroom
        .publish(
            &engine,
            keys.public_key(),
            EventBuilder::new(Kind::from(GROUP_KIND)).content("still wet"),
        )
        .expect("the darkroom publication is accepted");
    drain_until(&photographers_receipts, |status| {
        matches!(status, WriteStatus::Acked(_))
    });
    drain_until(&darkroom_receipts, |status| {
        matches!(status, WriteStatus::Acked(_))
    });

    let at_photographers = wait_for_events(&photographers_host, 1);
    let at_darkroom = wait_for_events(&darkroom_host, 1);
    assert_eq!(
        at_photographers.len(),
        1,
        "the photographers host receives exactly one event"
    );
    assert_eq!(
        at_darkroom.len(),
        1,
        "the darkroom host receives exactly one event"
    );
    assert_eq!(
        h_rows(&at_photographers[0]),
        vec!["photographers".to_string()],
        "the photographers host's event carries only the photographers h"
    );
    assert_eq!(
        h_rows(&at_darkroom[0]),
        vec!["darkroom".to_string()],
        "the darkroom host's event carries only the darkroom h"
    );
    assert_eq!(at_photographers[0].content, "first light");
    assert_eq!(at_darkroom[0].content, "still wet");

    // Give a late, WRONG delivery (the exact shape a cross-scope leak would
    // produce) a real chance to arrive before the negative assertion runs.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        photographers_host.admitted_events().len(),
        1,
        "the photographers host must never also receive the darkroom event"
    );
    assert_eq!(
        darkroom_host.admitted_events().len(),
        1,
        "the darkroom host must never also receive the photographers event"
    );

    engine.shutdown();
    photographers_host.shutdown();
    darkroom_host.shutdown();
}

// ===========================================================================
// PROTOCOL-PUBLISHINGTHROUGHTHEGROUP-006/-007 -- a multi-host group write
// preserves EXACT per-host outcomes, never flattened into "the host": one
// host acking and another rejecting the SAME publish call are two
// independent, separately observable ordinary facts, and neither host's
// outcome is tried anywhere outside the scope.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_multi_host_write_preserves_exact_per_host_outcomes_without_touching_anything_outside_the_scope(
) {
    let keys = Keys::generate();
    let acking_host = ScriptedRelay::start(&RelayConfig::default()).await;
    let rejecting_host = ScriptedRelay::start(&RelayConfig {
        reject_writes: Some("blocked: this host refuses every event".to_string()),
        ..RelayConfig::default()
    })
    .await;
    let engine = engine_with_signer(&keys);
    let group = nip29::on([acking_host.url.clone(), rejecting_host.url.clone()])
        .expect("two hosts form a scope")
        .group(GROUP_ID);

    let receipts = group
        .publish(
            &engine,
            keys.public_key(),
            EventBuilder::new(Kind::from(GROUP_KIND)).content("first light"),
        )
        .expect("the publish door accepts a two-host group write");

    // Drain until BOTH a per-host Acked and a per-host Rejected have landed,
    // as two SEPARATE ordinary facts -- never one flattened into the other.
    let deadline = Instant::now() + SETTLE;
    let mut seen = Vec::new();
    let (mut saw_ack, mut saw_reject) = (false, false);
    while !(saw_ack && saw_reject) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "never saw both an Acked and a Rejected fact; saw {seen:?}"
        );
        let status = receipts
            .recv_timeout(remaining)
            .unwrap_or_else(|error| panic!("receipt stream ended early ({error:?}); saw {seen:?}"));
        match &status {
            WriteStatus::Acked(relay) if *relay == acking_host.url => saw_ack = true,
            WriteStatus::Rejected(relay, _) if *relay == rejecting_host.url => saw_reject = true,
            _ => {}
        }
        seen.push(status);
    }

    assert!(
        seen.iter()
            .any(|status| matches!(status, WriteStatus::Acked(relay) if *relay == acking_host.url)),
        "the acking host's own outcome must be an ordinary Acked fact: {seen:?}"
    );
    assert!(
        seen.iter().any(|status| matches!(
            status,
            WriteStatus::Rejected(relay, message)
                if *relay == rejecting_host.url && message.contains("this host refuses")
        )),
        "the rejecting host's own outcome must be an ordinary Rejected fact carrying its own \
         message, never merged into the acking host's outcome: {seen:?}"
    );
    assert_eq!(
        relays_named_by(&seen),
        BTreeSet::from([acking_host.url.clone(), rejecting_host.url.clone()]),
        "every relay fact this write produced names exactly the scope's two hosts -- nothing \
         tried outside it because one host rejected"
    );

    let delivered = wait_for_events(&acking_host, 1);
    assert_eq!(delivered.len(), 1, "the acking host admitted the event");
    assert_eq!(
        rejecting_host.wire_record().event_ids.len(),
        1,
        "the rejecting host was attempted exactly once, never retried elsewhere"
    );

    engine.shutdown();
    acking_host.shutdown();
    rejecting_host.shutdown();
}
