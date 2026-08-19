//! First-value custody and restart survival for the kind:10009 saved-groups
//! list, proven through the direct Rust facade.
//!
//! Ported out of `nmp-parity` when that crate was deleted. `nmp-parity`
//! existed to run one scenario through both the Rust facade and the FFI
//! facade and compare them; most of it was FFI-projection proof with no
//! subject once the FFI facade went. This scenario is not: it drives ONE
//! facade and asserts engine behaviour -- a saved group accepted before the
//! relay has answered survives a close/reopen and is replayed over the later
//! real kind:10009 source without discarding anything that source owns. It
//! happened to be written against the FFI engine only because that is where
//! the harness lived.
//!
//! The other NIP-29 tests in this crate cover demand resolution and the
//! publication door. None of them covers first-value custody across a
//! restart, which is why this one is ported rather than dropped.

use std::time::{Duration, Instant};

use nmp::{Engine, EngineConfig, LiveQuery, ReceiptReattachment, RelayUrl, Subscription};
use nmp_nip29::{
    add_group_to_list, current_account_group_list_demand, group_list_capability, group_list_writes,
    SimpleGroupEntry,
};
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

const WAIT: Duration = Duration::from_secs(30);
const GROUP_LIST_KIND: u16 = 10_009;

/// Reachable but never answers the author's kind:10002 question, so routing
/// never retires and the write stays in the publish queue under its original
/// receipt instead of settling and being retired.
const NONANSWERING_INDEXER: &str = "wss://indexer.example";

fn outbox_provider() -> Option<Box<dyn nmp::AuthorRouteProvider>> {
    Some(Box::new(nmp_outbox::Nip65Outbox::new([
        RelayUrl::parse(NONANSWERING_INDEXER).expect("fixture indexer url parses")
    ])))
}

/// Keep the group-list observation alive for the duration of a phase. The
/// engine only asks the relay for the source it is being asked about, so the
/// subscription is what makes the request happen at all.
fn open_group_list_observation(engine: &Engine) -> Subscription {
    engine
        .observe(
            LiveQuery::single(current_account_group_list_demand()),
            None,
        )
        .expect("group-list observation opens the author-outbox source")
}

/// A saved group accepted with no relay-ready source truth survives a genuine
/// close/reopen, and the later real kind:10009 the relay eventually supplies
/// is rebased under rather than overwritten.
///
/// The relay holds the group-list query for 30s, so custody is entered
/// against nothing: the capability default supplies one complete pending
/// kind:10009. After the restart the relay returns a NEWER author-signed
/// kind:10009 carrying opaque content, another group, an `r` tag, a
/// deliberately MALFORMED `group` tag with no host, and an `x` tag NIP-29
/// does not own. Every one of those must survive verbatim into the successor
/// alongside the saved group, and the ORIGINAL receipt must still own it.
///
/// The malformed tag is the sharp end: a materializer that parsed the source
/// into its own model and re-serialized would silently drop it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_group_list_action_survives_restart_and_replays_over_later_truth() {
    let delayed = RelayConfig {
        query_delay: Some(Duration::from_secs(30)),
        ..RelayConfig::default()
    };
    let mut relay = ScriptedRelay::start(&delayed).await;
    let relay_port = relay.port();
    let relay_url = relay.url.clone();
    let author = Keys::generate();
    let saved_host = RelayUrl::parse("wss://saved-host.example").expect("saved host parses");
    let directory = tempfile::tempdir().expect("persistent NIP-29 fixture directory");
    let store_path = directory.path().join("nip29-first-value-replay.redb");
    let config = || EngineConfig {
        store_path: Some(store_path.to_string_lossy().into_owned()),
        app_relays: vec![relay_url.to_string()],
        fallback_relays: vec![],
        ..EngineConfig::default()
    };

    let engine = Engine::new_with_capabilities_and_routing(
        config(),
        vec![group_list_capability()],
        outbox_provider(),
    )
    .expect("persistent NIP-29 engine opens");
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("NIP-29 author registers");
    let observation = open_group_list_observation(&engine);
    assert!(
        relay.wait_query_for_kind(GROUP_LIST_KIND, WAIT).await,
        "the delayed relay holds the group-list source request before first-value custody"
    );

    let writes = group_list_writes();
    let receipt = add_group_to_list(
        &engine,
        &writes,
        SimpleGroupEntry {
            group_id: "research".to_string(),
            host_relay: saved_host.clone(),
            name: Some("Research".to_string()),
        },
    )
    .expect("first saved group enters ordinary custody without relay truth");
    let receipt_id = receipt.id;
    assert_eq!(
        engine.publish_queue(None, u8::MAX).unwrap()[0].receipt_id,
        receipt_id,
        "the first generation is owned by the ordinary receipt"
    );
    engine.shutdown();
    observation.cancel();
    drop(receipt);
    drop(engine);
    relay.disconnect().await;

    let reopened = Engine::new_with_capabilities_and_routing(
        config(),
        vec![group_list_capability()],
        outbox_provider(),
    )
    .expect("persistent NIP-29 engine reopens");
    reopened
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("NIP-29 author reattaches");
    let reopened_observation = open_group_list_observation(&reopened);
    assert!(matches!(
        reopened
            .reattach_receipt(receipt_id)
            .expect("ordinary receipt reattaches after restart"),
        ReceiptReattachment::Attached { .. }
    ));

    relay = ScriptedRelay::start_on_port(relay_port, &RelayConfig::default()).await;
    assert_eq!(relay.url, relay_url);
    let existing_group = Tag::parse([
        "group".to_string(),
        "remote".to_string(),
        "wss://remote-host.example".to_string(),
        "Remote".to_string(),
    ])
    .unwrap();
    let existing_relay = Tag::parse(["r", "wss://remote-relay.example"]).unwrap();
    let malformed = Tag::parse(["group", "malformed"]).unwrap();
    let unrelated = Tag::parse(["x", "remote-owned", "extra"]).unwrap();
    let relay_source = EventBuilder::new(Kind::Custom(GROUP_LIST_KIND), "opaque private content")
        .tags([
            existing_group.clone(),
            existing_relay.clone(),
            malformed.clone(),
            unrelated.clone(),
        ])
        .custom_created_at(Timestamp::from(
            Timestamp::now().as_secs().saturating_add(5),
        ))
        .sign_with_keys(&author)
        .expect("later NIP-29 source signs");
    relay.seed_signed_event(&relay_source).await;

    let deadline = Instant::now() + WAIT;
    let successor = loop {
        if let Some(event) = relay.admitted_events().into_iter().find(|event| {
            event.content == "opaque private content"
                && event.tags.iter().any(|tag| tag == &existing_group)
                && event.tags.iter().any(|tag| tag == &existing_relay)
                && event.tags.iter().any(|tag| tag == &malformed)
                && event.tags.iter().any(|tag| tag == &unrelated)
                && event.tags.iter().any(|tag| {
                    let row = tag.as_slice();
                    row.first().is_some_and(|cell| cell == "group")
                        && row.get(1).is_some_and(|cell| cell == "research")
                        && row.get(2).is_some_and(|cell| cell == saved_host.as_str())
                })
        }) {
            break event;
        }
        assert!(
            Instant::now() < deadline,
            "later relay truth never produced a preserving NIP-29 successor; admitted={:?}",
            relay.admitted_events()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let queue = reopened.publish_queue(None, u8::MAX).unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].receipt_id, receipt_id);
    assert_eq!(queue[0].event_id, successor.id);

    reopened.shutdown();
    reopened_observation.cancel();
    relay.shutdown();
}
