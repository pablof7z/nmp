//! #1105: what a group publication actually does on the wire.
//!
//! Three claims were carried by a static script and by prose until this file
//! existed (`features/routing/removed-routes.feature`,
//! `features/groups/publishing-through-the-group.feature`):
//!
//!   1. the app supplies event CONTENT ONLY -- it names no relay and writes
//!      no context tag;
//!   2. the group appends exactly one `h` row and mints
//!      `WriteRouting::Explicit([host])` itself;
//!   3. the host ALONE receives the event, and the author's own outbox --
//!      known, discovered, and a live `Auto` destination in the same
//!      process -- is never contacted for it.
//!
//! A grep can pin a constructor; only a delivery can prove where an event
//! went. So this runs the real `Engine` against real in-process relays over
//! real sockets, and the author's outbox is the relay the engine DISCOVERED
//! for itself from a kind:10002 it fetched from an indexer -- never a
//! configured lane and never an injected fact. That is what makes "untouched"
//! worth asserting: the same engine, in the same test, has already published
//! an ordinary `Auto` note to that exact relay.
//!
//! Falsifiers performed against this file (#1105):
//!   - widen the group's route in `nmp-nip29::Group::write_intent` from
//!     `Explicit([host])` to `Explicit([host, ...])` or to `Auto`: the
//!     untouched-outbox assertion goes red;
//!   - let the app spell the group tag or the route instead of the group:
//!     `crates/nmp/src/group.rs`'s door takes neither, which is what
//!     `scripts/check-routing-vocabulary.sh` pins structurally.
//!
//! Same version-shadowing precaution as `runtime_integration.rs`: never
//! `use nostr_relay_builder::prelude::*` -- `nmp-test-support` owns the
//! bridge between the two pinned `nostr` versions.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use nmp::mechanism::runtime::FifoReceiver;
use nmp::nip29::{Group, GroupContextError};
use nmp::{
    Engine, EngineConfig, GroupOperations, GroupPublishError, SignerError, SignerOp,
    SignerPublicKey, SignerSignedEvent, SignerUnsignedEvent, SigningCapability, WriteStatus,
};
use nmp_grammar::{Binding, Durability, EventBuilder, Filter, Identity, WriteIntent, WritePayload, WriteRouting};
use nmp_local_signer::LocalKeySigner;
use nmp_resolver::LiveQuery;
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::{Keys, Kind, RelayUrl, Tag, Timestamp, UnsignedEvent};

const GROUP_ID: &str = "photographers";
const GROUP_KIND: u16 = 9;

/// Long enough for a real discovery round trip on a loaded CI runner, short
/// enough that a genuine failure reports rather than hangs.
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

/// A minimal live engine with a real local signer for `keys`, and no
/// indexer/outbox wiring at all -- for capstones that only care about the
/// group's own explicit route, never about `Auto`/outbox behavior.
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

/// THE proof. One process, one engine, three relays, two publishes:
///
/// - an ordinary note routed `Auto` lands at the author's own write relay,
///   which the engine learned by fetching a kind:10002 from the indexer. The
///   author outbox is now a demonstrated, live destination.
/// - a note published THROUGH THE GROUP lands at the group's host, carrying
///   an `h` row the app never wrote, on a route the app never named -- and
///   the author outbox, still known and still healthy, receives nothing and
///   is not contacted at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_write_reaches_only_the_host_while_the_author_outbox_stays_untouched() {
    let keys = Keys::generate();

    let indexer = ScriptedRelay::start(&RelayConfig::default()).await;
    let outbox = ScriptedRelay::start(&RelayConfig::default()).await;
    let host = ScriptedRelay::start(&RelayConfig::default()).await;
    // The author's real relay list, published where the engine's own
    // discovery subscription looks. Nothing is injected into the router.
    indexer
        .seed_relay_list(&keys, &[outbox.url.to_string()], &[], 1_700_000_000)
        .await;

    let engine = engine_reading_lists_from(&indexer, &keys);

    // ---- leg 1: the author outbox is real ---------------------------------
    let auto_receipts = engine
        .publish(WriteIntent {
            payload: WritePayload::Event(EventBuilder::new(Kind::TextNote).content("ordinary")),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
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
    assert_eq!(ordinary[0].content, "ordinary");

    // ---- leg 2: the group write --------------------------------------------
    let contacts_before = outbox.contact_count();
    let group = Group::new(host.url.clone(), GROUP_ID);
    // Content only. No relay, no routing value, no tag: the door takes none
    // of them, so this call cannot express them.
    let receipts = group
        .publish(
            &engine,
            EventBuilder::new(Kind::from(GROUP_KIND)).content("first light"),
        )
        .expect("the group publication is accepted");
    let statuses = drain_until(&receipts, |status| matches!(status, WriteStatus::Acked(_)));

    let delivered = wait_for_events(&host, 1);
    assert_eq!(delivered.len(), 1, "the host received exactly one event");
    let event = &delivered[0];
    assert_eq!(event.content, "first light");
    assert_eq!(
        h_rows(event),
        vec![GROUP_ID.to_string()],
        "the group appended exactly one h row, carrying the group id"
    );
    assert_eq!(
        event.tags.len(),
        1,
        "the group contributed the h row and nothing else: {:?}",
        event.tags
    );
    assert!(
        event.verify().is_ok(),
        "the id and signature cover the bytes the h row is inside"
    );
    assert_eq!(
        relays_named_by(&statuses),
        BTreeSet::from([host.url.clone()]),
        "every relay fact this write produced names the host and nothing else"
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
    host.shutdown();
    outbox.shutdown();
    indexer.shutdown();
}

/// The route is minted from the host the group was constructed with, and it
/// consults nothing: no relay list has ever been fetched here, the indexer
/// is never asked for one, and the group write still lands. An `Auto` write
/// in this same state would park with `AwaitingRoute`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_write_needs_no_relay_list_and_asks_no_indexer_for_one() {
    let keys = Keys::generate();

    let indexer = ScriptedRelay::start(&RelayConfig::default()).await;
    let host = ScriptedRelay::start(&RelayConfig::default()).await;
    let engine = engine_reading_lists_from(&indexer, &keys);

    let group = Group::new(host.url.clone(), GROUP_ID);
    let receipts = group
        .publish(
            &engine,
            EventBuilder::new(Kind::from(GROUP_KIND)).content("no list needed"),
        )
        .expect("the group publication is accepted");
    let statuses = drain_until(&receipts, |status| matches!(status, WriteStatus::Acked(_)));

    assert!(
        !statuses
            .iter()
            .any(|status| matches!(status, WriteStatus::AwaitingRoute { .. })),
        "a group write never waits on a directory: {statuses:?}"
    );
    assert_eq!(
        relays_named_by(&statuses),
        BTreeSet::from([host.url.clone()]),
        "the route is exactly the host the group was constructed with"
    );
    assert_eq!(wait_for_events(&host, 1).len(), 1);
    assert!(
        !indexer.contacted(),
        "no relay list of mine was read for a group write"
    );

    engine.shutdown();
    host.shutdown();
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
    let group = Group::new(host.url.clone(), GROUP_ID);

    // Half 1: a caller-supplied h is refused LOCALLY -- the relay, which
    // rejects everything, is never even contacted for it.
    let refused = group.publish(
        &engine,
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
            EventBuilder::new(Kind::from(GROUP_KIND)).content("first light"),
        )
        .expect("an ordinary draft is accepted at the door");
    let statuses = drain_until(&receipts, |status| {
        matches!(status, WriteStatus::Rejected(_, _))
    });
    assert!(
        statuses
            .iter()
            .any(|status| matches!(status, WriteStatus::Rejected(relay, message)
                if relay == &host.url && message.contains("ordinary relay rejection"))),
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
    let group = Group::new(host.url.clone(), GROUP_ID);

    let receipts = group
        .publish(
            &engine,
            EventBuilder::new(Kind::from(GROUP_KIND)).content("first light"),
        )
        .expect("the door accepts the draft; the signer fails afterward");
    let statuses = drain_until(&receipts, |status| {
        matches!(status, WriteStatus::Failed(reason) if reason.to_lowercase().contains("sign"))
    });

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

// ===========================================================================
// PROTOCOL-PRESIGNEDPUBLICATION-001 (facade wire half) -- a pre-signed event
// reaches the host byte-identical: same id, same signature, same tags, in
// the caller's own order.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pre_signed_event_reaches_the_host_byte_identical_on_the_wire() {
    let me = Keys::generate();
    let author = Keys::generate();
    let host = ScriptedRelay::start(&RelayConfig::default()).await;
    let engine = engine_with_signer(&me);
    let group = Group::new(host.url.clone(), GROUP_ID);

    let pre_signed = UnsignedEvent::new(
        author.public_key(),
        Timestamp::from(1_700_000_000u64),
        Kind::from(GROUP_KIND),
        vec![Tag::parse(["h", GROUP_ID]).unwrap()],
        "first light, signed in advance".to_string(),
    )
    .sign_with_keys(&author)
    .expect("fixture keys sign cleanly");

    let receipts = group
        .publish_signed(&engine, pre_signed.clone())
        .expect("a correctly contextualized signed event is accepted");
    drain_until(&receipts, |status| matches!(status, WriteStatus::Acked(_)));

    let delivered = wait_for_events(&host, 1);
    assert_eq!(delivered.len(), 1);
    assert_eq!(
        delivered[0], pre_signed,
        "the delivered wire event must be byte-identical to what was signed -- \
         same id, same signature, same tags, same order"
    );

    engine.shutdown();
    host.shutdown();
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
    let engine = engine_with_signer(&me);
    let group = Group::new(host.url.clone(), GROUP_ID);

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

    let subscription = engine
        .observe(
            LiveQuery(group.demand(Filter {
                ids: Some(Binding::Literal(BTreeSet::from([known_id.to_hex()]))),
                ..Filter::default()
            })),
            None,
        )
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
        matched = frame.deltas.iter().any(|delta| {
            matches!(delta, nmp::mechanism::core::RowDelta::Added(row) if row.event.id == known_id)
        });
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
// PROTOCOL-PRESIGNEDPUBLICATION-006 (facade wire witness) -- the route
// follows the group, never the signature: a pre-signed event authored by
// SOMEONE ELSE still lands only at the group's host, and that author's own
// real, discoverable write relay is never contacted.
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
        .seed_relay_list(&author, &[author_outbox.url.to_string()], &[], 1_700_000_000)
        .await;

    let engine = engine_reading_lists_from(&indexer, &me);
    let group = Group::new(host.url.clone(), GROUP_ID);

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
    let engine = engine_with_signer(&me);
    let group = Group::new(host.url.clone(), GROUP_ID);

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
    // for an already-signed payload too (it is not "a signer was asked"
    // evidence -- see `nmp-bdd`'s `CountingSigner` for the same caveat), so
    // the falsifiable claim here is narrower and stronger: IF it appears, it
    // must name the exact id the caller already had, never a freshly minted
    // one.
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

/// The intent the group hands the one publish door, read directly: the app's
/// draft goes in, and what comes out carries the `h` row and
/// `Explicit([host])`. Cheap and headless, so the vocabulary claim is pinned
/// even if the socket-level tests above are ever narrowed.
#[test]
fn the_group_mints_explicit_over_its_own_host_and_nothing_else() {
    let host = RelayUrl::parse("wss://groups.example.com").expect("a well-formed host");
    let group = Group::new(host.clone(), GROUP_ID);
    let intent = group
        .write_intent(EventBuilder::new(Kind::from(GROUP_KIND)).content("first light"))
        .expect("a plain draft is contextualizable");

    match intent.routing {
        WriteRouting::Explicit(relays) => assert_eq!(relays, vec![host]),
        WriteRouting::Auto => panic!("a group write is Explicit over its host, never Auto"),
    }
    match intent.payload {
        WritePayload::Event(builder) => {
            let rows: Vec<Vec<String>> = builder
                .tags
                .iter()
                .map(|tag| tag.clone().to_vec())
                .collect();
            assert_eq!(rows, vec![vec!["h".to_string(), GROUP_ID.to_string()]]);
        }
        _ => panic!("a group draft is an Event payload"),
    }
}
