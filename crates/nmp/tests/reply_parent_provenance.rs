//! End-to-end falsifier for ROUTING-OUTBOXDEFAULT-009/010 (#1365).
//!
//! The mechanism unit test proves the reducer's exact set answer. This test
//! proves the public `Engine::publish` path turns that answer into real socket
//! work: the reply reaches both the configured app relay and the relay that
//! actually served its parent while unresolved author-route lookups remain
//! open, and it never reaches a live relay named only by authored hint text.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use nmp::{
    Binding, CacheMode, Demand, Engine, EngineConfig, Filter, Identity, LiveQuery,
    ReadRouting, RelayState, RowDelta, WriteFact, WriteIntent, WritePayload, WriteRouting,
};
use nmp_runtime::FifoReceiver;
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::{EventBuilder as NostrEventBuilder, Keys, Kind, Tag, Timestamp};

const SETTLE: Duration = Duration::from_secs(30);

type DestinationSnapshot = (BTreeSet<nostr::RelayUrl>, bool, BTreeSet<nostr::PublicKey>);
type PublicationWitness = (BTreeSet<nostr::RelayUrl>, Option<DestinationSnapshot>);

fn wait_until_parent_is_canonical(subscription: &nmp::Subscription, parent: nostr::EventId) {
    let deadline = Instant::now() + SETTLE;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("the parent never entered NMP's canonical store");
        }
        let frame = subscription
            .recv_timeout(remaining)
            .expect("the parent observation stays open");
        if frame.deltas.into_iter().any(|delta| {
            matches!(delta, RowDelta::Added(row) if row.id() == parent && !row.sources.is_empty())
        }) {
            return;
        }
    }
}

fn wait_for_published_relays(
    receipts: &FifoReceiver<WriteFact>,
    expected: &BTreeSet<nostr::RelayUrl>,
) -> PublicationWitness {
    let deadline = Instant::now() + SETTLE;
    let mut published = BTreeSet::new();
    let mut destinations = None;
    let mut seen = Vec::new();
    while !expected.is_subset(&published) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "expected relays never published; expected={expected:?} published={published:?} seen={seen:?}"
            );
        }
        match receipts.recv_timeout(remaining) {
            Ok(WriteFact::Destinations {
                relays,
                complete,
                awaiting_author_routes,
            }) => {
                destinations = Some((relays.clone(), complete, awaiting_author_routes.clone()));
                seen.push(WriteFact::Destinations {
                    relays,
                    complete,
                    awaiting_author_routes,
                });
            }
            Ok(WriteFact::Relay {
                event_id,
                relay,
                state: RelayState::Published,
            }) => {
                published.insert(relay.clone());
                seen.push(WriteFact::Relay {
                    event_id,
                    relay,
                    state: RelayState::Published,
                });
            }
            Ok(fact) => seen.push(fact),
            Err(error) => panic!("the receipt stream ended early ({error:?}); saw {seen:?}"),
        }
    }
    (published, destinations)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn verified_parent_provenance_becomes_a_real_lane_while_raw_hint_text_does_not() {
    let app_relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let parent_relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let hinted_relay = ScriptedRelay::start(&RelayConfig::default()).await;

    let parent_author = Keys::generate();
    let parent = NostrEventBuilder::new(Kind::TextNote, "the parent")
        .custom_created_at(Timestamp::from(1_700_000_000))
        .sign_with_keys(&parent_author)
        .expect("sign parent fixture");
    parent_relay.seed_signed_event(&parent).await;

    let engine = Engine::new(EngineConfig {
        app_relays: vec![app_relay.url.to_string()],
        // Parent provenance and the forged hint are both third-party data.
        // Admit loopback deliberately so the negative control would really
        // connect if routing ever trusted the hint.
        ..EngineConfig::default()
    })
    .expect("build a temporary Redb engine");

    let mut parent_demand = Demand::new(
        Filter {
            ids: Some(Binding::Literal(BTreeSet::from([parent.id.to_hex()]))),
            ..Filter::default()
        },
        ReadRouting::Explicit(vec![parent_relay.url.clone()])
    )
    .expect("one pinned parent query is valid");
    parent_demand.cache = CacheMode::Strict;
    let parent_subscription = engine
        .observe(LiveQuery::single(parent_demand), None)
        .expect("observe the parent through the public facade");
    wait_until_parent_is_canonical(&parent_subscription, parent.id);

    let author = Keys::generate();
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("register and select the publishing identity");

    let parent_hex = parent.id.to_hex();
    let hinted_url = hinted_relay.url.to_string();
    let reply = nmp::EventBuilder::new(Kind::TextNote)
        .content("the reply")
        .tag(
            Tag::parse(["e", parent_hex.as_str(), hinted_url.as_str(), "reply"])
                .expect("a well-formed NIP-10 reply tag"),
        )
        .tag(Tag::public_key(parent_author.public_key()));
    let tracked = engine
        .publish(WriteIntent {
            payload: WritePayload::Event(reply),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
        })
        .expect("the reply enters NMP custody");

    let expected = BTreeSet::from([app_relay.url.clone(), parent_relay.url.clone()]);
    let (published, destinations) = wait_for_published_relays(&tracked.statuses, &expected);
    assert_eq!(
        published, expected,
        "only the two evidence-owned lanes publish"
    );

    let (relays, complete, awaiting) =
        destinations.expect("the receipt exposes the current routing picture");
    assert_eq!(relays, expected);
    assert!(
        !complete,
        "known lanes publish independently while author-route discovery remains unresolved"
    );
    assert_eq!(
        awaiting,
        BTreeSet::from([author.public_key(), parent_author.public_key()]),
        "the app can see exactly which author-route lookups are still open"
    );

    assert_eq!(
        app_relay
            .admitted_events()
            .iter()
            .map(|event| event.content.as_str())
            .collect::<Vec<_>>(),
        vec!["the reply"],
        "the configured app relay receives the reply"
    );
    assert_eq!(
        parent_relay
            .admitted_events()
            .iter()
            .map(|event| event.content.as_str())
            .collect::<Vec<_>>(),
        vec!["the reply"],
        "the verified parent source receives the reply"
    );
    assert!(
        hinted_relay.admitted_events().is_empty() && !hinted_relay.contacted(),
        "a relay named only in authored hint text is neither dialed nor written"
    );

    engine.shutdown();
    app_relay.shutdown();
    parent_relay.shutdown();
    hinted_relay.shutdown();
}
