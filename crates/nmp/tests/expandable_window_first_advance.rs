//! #1886 / Canary C6: the FIRST `request_rows` on an expandable window must
//! place its older-range REQ on a real socket.
//!
//! The engine-level falsifier in `nmp-engine/tests/core_headless/
//! expandable_window_advance.rs` proves the reducer now ARMS wire admission
//! for a staged advance. It cannot prove the runtime ever sees that arm: it
//! models `Cmd::RequestRows`'s dispatch itself, and the defect being fixed
//! was precisely that the real arm discarded the staged turn's effects on the
//! success path while dispatching them on failure. Only the production
//! runtime thread, driven through the public `Handle` against a relay that
//! reports its own wire record, closes that loop.
//!
//! nmp:falsifier=The first advance of an expandable window reaches the relay,
//! proven by the relay's own record of the REQ frames it received -- not by
//! engine effects the test itself chose to dispatch.

use std::collections::BTreeSet;
use std::time::Duration;

use nmp_engine::core::HistoryQuery;
use nmp_grammar::{Binding, Demand, Filter, LiveQuery, ReadRouting};
use nmp_runtime::EngineThread;
use nmp_store::{RedbStore, RelayObserved};
use nmp_test_support::relays::{AdvertisedLimits, RelayConfig, ScriptedRelay};
use nmp_transport::PoolConfig;
use nostr::{Event, Keys, Kind, RelayUrl, Timestamp, UnsignedEvent};

fn note(keys: &Keys, created_at: u64, content: &str) -> Event {
    UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(created_at),
        Kind::TextNote,
        Vec::new(),
        content,
    )
    .sign_with_keys(keys)
    .expect("fixture signing never fails")
}

/// A window over one author's notes, pinned to `relay` so no routing facts
/// are involved in whether the advance has somewhere to send its REQ.
fn window(author: &Keys, relay: &RelayUrl, initial: usize, max: usize) -> HistoryQuery {
    HistoryQuery::new(
        LiveQuery::single(
            Demand::new(
                Filter {
                    kinds: Some(BTreeSet::from([1u16])),
                    authors: Some(Binding::Literal(BTreeSet::from([author
                        .public_key()
                        .to_hex()]))),
                    ..Filter::default()
                },
                ReadRouting::Explicit(vec![relay.clone()]),
            )
            .expect("a pinned literal demand is constructible"),
        ),
        initial,
        max,
    )
}

/// The `until` a REQ's filters name, if any. The window's three REQ roles are
/// distinguishable by it alone: the initial acquisition names none, the
/// tie-second REQ names the boundary second, and the older-range REQ names
/// one second below the boundary.
fn until_of(filters: &[serde_json::Value]) -> BTreeSet<u64> {
    filters
        .iter()
        .filter_map(|filter| filter.get("until").and_then(serde_json::Value::as_u64))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_advance_of_a_window_reaches_the_relay() {
    let relay = ScriptedRelay::start(&RelayConfig {
        advertised_limits: Some(AdvertisedLimits::default()),
        ..RelayConfig::default()
    })
    .await;
    let author = Keys::generate();

    // Six local rows at 100..=105. The window opens on the newest three, so
    // its boundary is 103 and the first advance's older range is `until:102`.
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    for seq in 0..6u64 {
        store
            .insert(
                note(&author, 100 + seq, &format!("note-{seq}")),
                RelayObserved::new(relay.url.clone(), Timestamp::from(200u64)),
            )
            .expect("seeding the window's local rows");
    }

    let (engine_thread, handle) =
        EngineThread::spawn(store, 10, PoolConfig::default()).expect("spawn runtime");
    let (session, _frames) = handle
        .subscribe_history(window(&author, &relay.url, 3, 6))
        .expect("open the window");

    // Settle the connection FIRST. A relay that connects only after the
    // advance is staged masks the whole defect: `RelayConnected` runs a full
    // recompile, which places every pending atom -- including the advance's --
    // without any admission arm being involved. Waiting for the window's own
    // opening REQ and then for a quiet wire reproduces the real scroll, where
    // the socket has been up and idle for as long as the user has been
    // reading.
    assert!(
        relay
            .wait_wire_req(Duration::from_secs(10), |req| until_of(&req.filters)
                .is_empty())
            .await
            .is_some(),
        "the window's opening acquisition never reached the relay"
    );
    relay
        .wait_wire_quiet(Duration::from_millis(200), Duration::from_secs(5))
        .await;

    assert_eq!(
        handle.request_rows(session, 6),
        Some(Ok(())),
        "the first advance must be accepted"
    );

    let older = relay
        .wait_wire_req(Duration::from_secs(10), |req| {
            until_of(&req.filters).contains(&102)
        })
        .await;
    let record = relay.wire_record();
    assert!(
        older.is_some(),
        "the first advance owes the relay an older-range REQ until 102; every \
         REQ the relay actually received was {:#?}",
        record
            .reqs
            .iter()
            .map(|req| (req.sub_id.clone(), req.filters.clone()))
            .collect::<Vec<_>>()
    );

    handle.unsubscribe_history(session);
    handle.shutdown();
    engine_thread.join();
    relay.shutdown();
}
