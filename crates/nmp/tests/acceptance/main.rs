//! Deterministic public-facade acceptance capstones.
//!
//! This target constructs the product through `Engine::new`, drives public
//! queries, observes public frames, and uses scripted relays only as
//! independent world/wire witnesses. Mechanism handles and fixture routing
//! facts do not exist in this target.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use cucumber::{given, then, when, World as _};
use nmp::{Binding, Engine, EngineConfig, Filter, LiveQuery, Subscription};
use nmp_test_support::relays::{RelayConfig, ScriptedRelay, WireReq};
use nostr::{EventBuilder, EventId, Keys, Timestamp};

const WAIT: Duration = Duration::from_secs(10);
const NOTE: &str = "hello from Alice over her discovered relay";

#[derive(cucumber::World, Default)]
struct AcceptanceWorld {
    alice: Option<Keys>,
    expected_note: Option<EventId>,
    indexer: Option<ScriptedRelay>,
    content: Option<ScriptedRelay>,
    engine: Option<Engine>,
    subscription: Option<Subscription>,
    witnessed_indexer_request: Option<WireReq>,
    content_uncontacted_before_route: bool,
    witnessed_content_request: Option<WireReq>,
    content_row_arrived: bool,
}

impl std::fmt::Debug for AcceptanceWorld {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcceptanceWorld")
            .field("alice_staged", &self.alice.is_some())
            .field("indexer_started", &self.indexer.is_some())
            .field("content_relay_started", &self.content.is_some())
            .field("engine_started", &self.engine.is_some())
            .field("content_row_arrived", &self.content_row_arrived)
            .finish()
    }
}

impl AcceptanceWorld {
    fn alice(&self) -> &Keys {
        self.alice.as_ref().expect("Alice is staged")
    }

    fn indexer(&self) -> &ScriptedRelay {
        self.indexer.as_ref().expect("the indexer is staged")
    }

    fn content(&self) -> &ScriptedRelay {
        self.content.as_ref().expect("the content relay is staged")
    }

    fn cleanup(&mut self) {
        self.subscription.take();
        if let Some(engine) = self.engine.take() {
            engine.shutdown();
        }
        if let Some(relay) = self.indexer.take() {
            relay.shutdown();
        }
        if let Some(relay) = self.content.take() {
            relay.shutdown();
        }
    }
}

#[given("Alice's note exists only at her content relay")]
async fn alice_note_exists_only_at_content_relay(world: &mut AcceptanceWorld) {
    let alice = Keys::generate();
    let content = ScriptedRelay::start(&RelayConfig::default()).await;
    let note = EventBuilder::text_note(NOTE)
        .custom_created_at(Timestamp::from(2))
        .sign_with_keys(&alice)
        .expect("fixture note signs");
    let expected_note = note.id;
    content.seed_signed_event(&note).await;

    world.alice = Some(alice);
    world.expected_note = Some(expected_note);
    world.content = Some(content);
}

#[given("Alice's relay list will be published only by the configured indexer")]
async fn alice_relay_list_exists_only_at_indexer(world: &mut AcceptanceWorld) {
    assert!(
        world.content.is_some(),
        "the content relay must be staged before the indexer"
    );
    world.indexer = Some(ScriptedRelay::start(&RelayConfig::default()).await);
}

#[when("a cold public engine observes Alice's notes")]
async fn cold_public_engine_observes_alices_notes(world: &mut AcceptanceWorld) {
    let indexer_url = world.indexer().url.to_string();
    let content_url = world.content().url.to_string();
    let alice_hex = world.alice().public_key().to_hex();
    let expected_note = world.expected_note.expect("the note is staged");

    let engine = Engine::new(EngineConfig {
        indexer_relays: vec![indexer_url],
        ..EngineConfig::default()
    })
    .expect("the public engine starts");
    let query = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1])),
        authors: Some(Binding::Literal(BTreeSet::from([alice_hex.clone()]))),
        ..Filter::default()
    });
    let subscription = engine
        .observe(query, None)
        .expect("the public observation starts");

    let indexer_request = world
        .indexer()
        .wait_wire_req(WAIT, |req| {
            req.kinds().contains(&10_002) && req.authors().contains(&alice_hex)
        })
        .await
        .expect("the independent indexer wire never saw Alice's exact kind:10002 REQ");
    world.content_uncontacted_before_route =
        !world.content().contacted() && world.content().connection_count() == 0;
    world.witnessed_indexer_request = Some(indexer_request);

    world
        .indexer()
        .seed_relay_list(
            world.alice(),
            std::slice::from_ref(&content_url),
            std::slice::from_ref(&content_url),
            1,
        )
        .await;

    world.witnessed_content_request = Some(
        world
            .content()
            .wait_wire_req(WAIT, |req| {
                req.kinds().contains(&1) && req.authors().contains(&alice_hex)
            })
            .await
            .expect("the independent content-relay wire never saw Alice's exact kind:1 REQ"),
    );

    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline && !world.content_row_arrived {
        if let Ok(frame) = subscription.recv_timeout(Duration::from_millis(250)) {
            world.content_row_arrived = frame.deltas.iter().any(|delta| {
                delta
                    .row()
                    .and_then(|row| row.signed_event())
                    .is_some_and(|event| event.id == expected_note)
                    && matches!(
                        delta,
                        nmp::RowDelta::Added(row)
                            if row.sources().contains(&world.content().url)
                    )
            });
        }
    }
    world.engine = Some(engine);
    world.subscription = Some(subscription);
}

#[then("the indexer was asked for Alice's relay list")]
fn indexer_was_asked_for_relay_list(world: &mut AcceptanceWorld) {
    assert!(
        world.witnessed_indexer_request.is_some(),
        "the independent indexer witness saw no Alice-scoped kind:10002 REQ"
    );
}

#[then("the content relay was not contacted before that relay list arrived")]
fn content_relay_waited_for_route(world: &mut AcceptanceWorld) {
    assert!(
        world.content_uncontacted_before_route,
        "the content relay was contacted before the route fact existed"
    );
}

#[then("Alice's content was fetched from her discovered relay")]
fn alice_content_arrived_from_discovered_relay(world: &mut AcceptanceWorld) {
    assert!(
        world.witnessed_content_request.is_some(),
        "the independent content-relay witness saw no Alice-scoped kind:1 REQ"
    );
    assert!(
        world.content_row_arrived,
        "the public subscription never delivered Alice's staged row"
    );
}

#[then("the indexer was never used as a generic content fallback")]
fn indexer_was_not_content_fallback(world: &mut AcceptanceWorld) {
    assert!(
        world
            .indexer()
            .wire_record()
            .reqs
            .iter()
            .all(|req| !req.kinds().contains(&1)),
        "the independent indexer witness saw a generic kind:1 REQ"
    );
}

#[test]
fn public_engine_bootstraps_author_route_before_content_fetch() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("acceptance runtime")
        .block_on(async {
            let feature = PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../features/routing/cold-nip65-discovery.feature"
            ));
            AcceptanceWorld::cucumber()
                .max_concurrent_scenarios(1)
                .after(|_feature, _rule, _scenario, _finished, world| {
                    Box::pin(async move {
                        if let Some(world) = world {
                            world.cleanup();
                        }
                    })
                })
                .run_and_exit(feature)
                .await;
        });
}
