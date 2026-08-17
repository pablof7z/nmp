//! Boot recovery must cost durable commits proportional to what CHANGES,
//! never to how much durable write state exists (#889).
//!
//! The Mosaico laptop incident is the shape these falsifiers pin: a store
//! holding 15,311 open intents made `add_account` block for more than 53
//! seconds, because the engine thread rebuilds ownership before it reads its
//! first command and that rebuild committed one fsync-durable transaction per
//! intent — plus one more per `Eligible` lane it re-parked, none of which
//! recorded a fact that was not already durable.
//!
//! Revision numbers are what make the claim instrumentation-free: every lane
//! transition bumps `PublishQueueLane::revision`, so an identical durable lane
//! set either side of a boot is proof that boot wrote nothing.

use nmp_engine::core::{EngineCore, EngineMsg};
use nmp_grammar::{Identity, WriteIntent, WritePayload, WriteRouting};
use nmp_store::{PublishQueueLane, PublishQueueLaneState, RedbStore};
use nostr::{Event, EventBuilder, Keys, Kind, RelayUrl, Timestamp};

/// One pre-signed durable write, so population never needs a signer round
/// trip and every recovered intent is `Signed` — the exact shape that makes
/// boot re-resolve routes and bootstrap lanes.
fn signed(keys: &Keys, kind: Kind, content: &str, created_at: u64) -> Event {
    EventBuilder::new(kind, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign fixture")
}

/// Accept `count` durable writes routed at exactly `relay`, which nothing ever
/// connects to, so every lane stays owned across the restart.
fn populate(path: &std::path::Path, keys: &Keys, relay: &RelayUrl, count: usize) {
    let store = RedbStore::open(path).expect("open population store");
    let mut core = EngineCore::new(store, count + 1);
    core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
    for i in 0..count {
        core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Signed(signed(
                keys,
                Kind::TextNote,
                &format!("boot recovery {i}"),
                1_000_000 + i as u64,
            )),
            routing: WriteRouting::Explicit(vec![relay.clone()]),
            identity: Identity::Active,
        }));
    }
}

/// Every durable lane in the store, ordered, with its revision — the exact
/// value a rewrite would change.
fn lane_snapshot(store: &RedbStore) -> Vec<PublishQueueLane> {
    let mut lanes: Vec<PublishQueueLane> = store
        .recover_publish_queue()
        .expect("recover intents")
        .into_iter()
        .flat_map(|intent| {
            store
                .recover_publish_queue_lanes(intent.intent_id)
                .expect("recover lanes")
        })
        .collect();
    lanes.sort_by(|left, right| left.key.cmp(&right.key));
    lanes
}

/// Drive every lane in the store to `Eligible`, the state the incident's
/// store was in: a relay that was reachable once, so the lanes were woken,
/// and then was not, so nothing consumed them.
fn make_every_lane_eligible(store: &mut RedbStore) {
    let intents = store.recover_publish_queue().expect("recover intents");
    for intent in intents {
        let lanes = store
            .recover_publish_queue_lanes(intent.intent_id)
            .expect("recover lanes");
        for lane in lanes {
            store
                .set_lane_eligible(&lane.key, lane.revision, Timestamp::from(1_000_000))
                .expect("force lane eligible");
        }
    }
}

#[test]
fn boot_recovery_rewrites_no_lane_when_no_durable_fact_changed() {
    const INTENTS: usize = 300;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("boot-recovery-bound.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://boot-recovery-bound.example").unwrap();

    populate(&path, &keys, &relay, INTENTS);
    {
        let mut store = RedbStore::open(&path).unwrap();
        make_every_lane_eligible(&mut store);
    }

    let before = {
        let store = RedbStore::open(&path).unwrap();
        lane_snapshot(&store)
    };
    assert_eq!(before.len(), INTENTS, "one lane per intent");
    assert!(
        before
            .iter()
            .all(|lane| matches!(lane.state, PublishQueueLaneState::Eligible { .. })),
        "the fixture is the disconnected-Eligible population"
    );

    {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(store, INTENTS + 1);
        core.recover_on_boot();
    }
    let after = {
        let store = RedbStore::open(&path).unwrap();
        lane_snapshot(&store)
    };

    let rewritten: Vec<_> = before
        .iter()
        .zip(&after)
        .filter(|(before, after)| before != after)
        .map(|(before, after)| {
            format!(
                "{} revision {} {:?} -> revision {} {:?}",
                before.key.relay, before.revision, before.state, after.revision, after.state
            )
        })
        .collect();
    assert!(
        rewritten.is_empty(),
        "boot recovery rewrote {} of {INTENTS} lanes that had nothing new to record: {:#?}",
        rewritten.len(),
        &rewritten[..rewritten.len().min(4)]
    );
}

/// The reproducible before/after for #889's headline symptom, run on demand:
///
/// ```text
/// cargo test --release -p nmp --test boot_recovery_bound \
///     measure_add_account_behind_boot_recovery -- --ignored --nocapture
/// ```
///
/// It measures the exact consumer-visible number the issue reports: how long
/// `Engine::add_account` blocks when it is the first command a freshly opened
/// engine receives over a large durable-write state. `add_account` sends one
/// `Cmd` to the single engine thread and waits for the reply, and that thread
/// runs boot recovery to completion before its first `recv`, so the two
/// latencies are the same latency.
///
/// `#[ignore]`d because it builds a real on-disk store with thousands of
/// intents and reports wall-clock numbers, neither of which belongs in the
/// ordinary suite.
#[test]
#[ignore = "manual before/after latency qualification"]
fn measure_add_account_behind_boot_recovery() {
    const INTENTS: usize = 4_000;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("measure-boot-recovery.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://measure-boot-recovery.invalid").unwrap();

    let started = std::time::Instant::now();
    populate(&path, &keys, &relay, INTENTS);
    let populated = started.elapsed();
    {
        let mut store = RedbStore::open(&path).unwrap();
        make_every_lane_eligible(&mut store);
    }

    let started = std::time::Instant::now();
    {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(store, INTENTS + 1);
        core.recover_on_boot();
    }
    let recovery = started.elapsed();

    let engine = nmp::Engine::new(nmp::EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..nmp::EngineConfig::default()
    })
    .expect("open engine over the populated store");
    let started = std::time::Instant::now();
    let keys = Keys::generate();
    engine
        .add_private_key_account(&keys.secret_key().to_secret_bytes(), false)
        .expect("register an account");
    let add_account = started.elapsed();

    println!(
        "measure_add_account_behind_boot_recovery intents={INTENTS} (RedbStore)\n  \
         populate:              {populated:?}\n  \
         recover_on_boot:       {recovery:?}\n  \
         add_account (blocked): {add_account:?}"
    );
}

/// The other half of what made the incident store large: a presence renewal
/// loop against a relay nothing ever reached.
///
/// Every renewal wins the same `(author, kind:30315, d)` address, and an older
/// obligation there that never put a byte on the wire is worth nothing to
/// anybody — the relay would drop it for the newer one. Acceptance retires it
/// in the same transaction that installs the winner, so what boot recovers is
/// one obligation rather than one per renewal, and the recovery bound above
/// never has to be spent on obligations that are already obsolete.
#[test]
fn presence_renewals_leave_exactly_one_open_obligation() {
    const RENEWALS: usize = 200;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("presence-renewals.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://presence-renewals.example").unwrap();

    {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(store, RENEWALS + 1);
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        for i in 0..RENEWALS {
            let event = EventBuilder::new(Kind::from(30315u16), format!("online {i}"))
                .tag(nostr::Tag::identifier("general"))
                .custom_created_at(Timestamp::from(2_000_000 + i as u64))
                .sign_with_keys(&keys)
                .expect("sign presence fixture");
            core.handle(EngineMsg::Publish(WriteIntent {
                payload: WritePayload::Signed(event),
                routing: WriteRouting::Explicit(vec![relay.clone()]),
                identity: Identity::Active,
            }));
        }
    }

    let store = RedbStore::open(&path).unwrap();
    let open = store.recover_publish_queue().expect("recover intents");
    assert_eq!(
        open.len(),
        1,
        "{RENEWALS} renewals at one address must leave one open obligation, not {}",
        open.len()
    );
    let (frozen, _, _, _) = open[0].event_work().expect("ordinary event work");
    assert_eq!(
        frozen.content, "online 199",
        "the surviving obligation is the newest renewal"
    );
}
