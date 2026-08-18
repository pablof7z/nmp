//! #680 — a stalled snapshot consumer stays memory-bounded and never delays the
//! engine or an unrelated active consumer (falsifier 3 / measurement item 6).
//!
//! Two observations of the same query share one engine. One is never polled
//! ("stalled") while many state changes are driven; the other polls each change.
//! We measure the active consumer's per-change latency (it must stay low —
//! independent of the stalled one) and prove the stalled consumer's mailbox is
//! bounded: on resume it converges to the newest exact state in a small,
//! bounded number of frames (conflation), NOT one queued frame per change.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use nmp_ffi::facade::{NmpEngine, NmpEngineConfig, NmpRowStream};
use nmp_ffi::session::FfiPrivateKey;
use nmp_ffi::types::{
    FfiCacheMode, FfiDemand, FfiFilter, FfiFrame, FfiFreshness, FfiIdentity, FfiLiveQuery,
    FfiReadRouting, FfiRowDelta, FfiWriteIntent, FfiWritePayload, FfiWriteRouting,
};

const TEST_SECRET_KEY_HEX: &str =
    "0000000000000000000000000000000000000000000000000000000000000001";
const CHANGES: usize = 400;

fn note_query() -> FfiLiveQuery {
    FfiLiveQuery {
        branches: vec![FfiDemand {
            selection: FfiFilter {
                kinds: Some(vec![1]),
                ..FfiFilter::default()
            },
            routing: FfiReadRouting::Auto,
            authenticate_as: None,
            cache: FfiCacheMode::Agnostic,
            freshness: FfiFreshness::Live,
        }],
        aggregate_result_limit: None,
    }
}

fn added_ids(frame: &FfiFrame, into: &mut BTreeSet<String>) {
    for delta in &frame.deltas {
        if let FfiRowDelta::Added { row } = delta {
            into.insert(row.id.clone());
        }
    }
}

async fn next_committed(stream: &NmpRowStream) -> Option<FfiFrame> {
    let pull = stream.begin_next().expect("row ticket begins");
    let frame = pull.receive().await.expect("row ticket lifecycle is valid");
    pull.commit().expect("delivered row ticket commits");
    frame
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_consumer_is_bounded_and_does_not_delay_active_consumer_or_engine() {
    let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine builds");
    let keys = nostr::Keys::parse(TEST_SECRET_KEY_HEX).unwrap();
    // Make the account active so durable unsigned publishes sign locally and
    // become query-visible rows.
    engine
        .add_private_key_account(
            FfiPrivateKey::from_bytes(keys.secret_key().to_secret_bytes().to_vec()).unwrap(),
            true,
        )
        .expect("activate account");

    // Two observations of the SAME live query.
    let stalled = engine.observe(note_query(), None).expect("opens"); // never polled until the end
    let active = engine.observe(note_query(), None).expect("opens");

    // Drain the active consumer's initial current-state frame so subsequent
    // frames correspond to the changes we drive.
    let _ = tokio::time::timeout(Duration::from_secs(2), next_committed(&active)).await;

    let mut active_rows: BTreeSet<String> = BTreeSet::new();
    let mut max_active_latency = Duration::ZERO;

    for i in 0..CHANGES {
        engine
            .publish(FfiWriteIntent {
                payload: FfiWritePayload::Event {
                    builder: nmp_ffi::types::FfiEventBuilder {
                        kind: 1,
                        tags: Vec::new(),
                        content: format!("note-{i}"),
                        created_at: Some(1_700_000_000 + i as u64),
                    },
                },
                routing: FfiWriteRouting::Explicit {
                    relays: vec!["wss://write.example".to_string()],
                },
                identity: FfiIdentity::Active,
            })
            .expect("publish accepted");

        // The active consumer must observe progress promptly — its latency must
        // be independent of the never-polled stalled consumer.
        let started = Instant::now();
        let frame = tokio::time::timeout(Duration::from_secs(5), next_committed(&active))
            .await
            .expect("active consumer is NOT delayed by the stalled one");
        max_active_latency = max_active_latency.max(started.elapsed());
        if let Some(frame) = frame {
            added_ids(&frame, &mut active_rows);
        }
    }

    // The engine made progress independent of the stalled consumer: the active
    // consumer accumulated (essentially) all of the newly-created rows.
    assert!(
        active_rows.len() >= CHANGES * 9 / 10,
        "active consumer kept up while the other was stalled: saw {} of {CHANGES}",
        active_rows.len()
    );

    // Now resume the stalled consumer. Its single-slot mailbox conflated every
    // skipped change into ONE pending transition, so it converges to the newest
    // exact state (all CHANGES rows) in a BOUNDED number of frames — not one
    // queued frame per change.
    let mut stalled_rows: BTreeSet<String> = BTreeSet::new();
    let mut frames = 0usize;
    while stalled_rows.len() < CHANGES && frames < 32 {
        match tokio::time::timeout(Duration::from_secs(3), next_committed(&stalled)).await {
            Ok(Some(frame)) => {
                frames += 1;
                added_ids(&frame, &mut stalled_rows);
            }
            _ => break,
        }
    }

    eprintln!("\n#680 stalled-consumer isolation ({CHANGES} state changes):");
    eprintln!("  active consumer   : max per-change latency = {max_active_latency:?}");
    eprintln!(
        "  stalled consumer  : converged to {} rows in {} frame(s) (would be {CHANGES} queued frames if unbounded)",
        stalled_rows.len(),
        frames
    );

    assert_eq!(
        stalled_rows.len(),
        CHANGES,
        "resumed stalled consumer reaches the newest exact state"
    );
    assert!(
        frames <= 8,
        "stalled consumer converged in a bounded number of frames (mailbox conflation), \
         not {CHANGES} queued frames; took {frames}"
    );
    assert!(
        max_active_latency < Duration::from_secs(2),
        "active consumer latency stayed low while another consumer was stalled: {max_active_latency:?}"
    );

    engine.shutdown();
}
