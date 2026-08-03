//! Isolated physical-representation benchmark for issue #1027.
//!
//! Population and recovery run as separate processes. The fixture fixes the
//! semantic lane count and transaction sequence; this benchmark changes no
//! scheduler policy. Typical use:
//!
//! ```text
//! publish_queue_recovery_bench populate store.redb 1000 4
//! publish_queue_recovery_bench recover store.redb 1000 4
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use nmp_grammar::CorrelationToken;
use nmp_store::{
    sentinel_signature, AcceptWrite, EventStore, HandoffEvidence, IntentSigState,
    PublishQueueAttemptHandoff, PublishQueuePostHandoffState, RedbStore, WriteDurability,
};
use nostr::{Event, EventBuilder, Keys, Kind, RelayUrl, Timestamp};
use serde::Serialize;

struct CountingAllocator;

static ALLOCATION_OPS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATION_OPS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATION_OPS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Clone, Copy)]
struct Counters {
    allocation_ops: u64,
    allocated_bytes: u64,
    process_write_bytes: Option<u64>,
}

impl Counters {
    fn sample() -> Self {
        Self {
            allocation_ops: ALLOCATION_OPS.load(Ordering::Relaxed),
            allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
            process_write_bytes: process_write_bytes(),
        }
    }

    fn delta(self, before: Self) -> Self {
        Self {
            allocation_ops: self.allocation_ops.saturating_sub(before.allocation_ops),
            allocated_bytes: self.allocated_bytes.saturating_sub(before.allocated_bytes),
            process_write_bytes: self
                .process_write_bytes
                .zip(before.process_write_bytes)
                .map(|(after, before)| after.saturating_sub(before)),
        }
    }
}

#[derive(Serialize)]
struct BenchResult {
    schema: &'static str,
    phase: &'static str,
    intents: usize,
    relays_per_intent: usize,
    lanes: usize,
    expected_commits: usize,
    wall_ns: u64,
    allocation_ops: u64,
    allocated_bytes: u64,
    process_write_bytes: Option<u64>,
    database_logical_bytes: u64,
    database_allocated_bytes: u64,
    normalized_semantic_bytes: usize,
    scheduler_effect_digest: String,
}

fn process_write_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/self/io")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("write_bytes:")?.trim().parse().ok())
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().try_into().unwrap_or(u64::MAX)
}

fn database_bytes(path: &Path) -> (u64, u64) {
    let metadata = std::fs::metadata(path).expect("database metadata");
    (metadata.len(), metadata.blocks().saturating_mul(512))
}

fn fixed_keys() -> Keys {
    Keys::parse("000000000000000000000000000000000000000000000000000000000000002a")
        .expect("fixed benchmark key")
}

fn signed_event(keys: &Keys, intent: usize) -> Event {
    EventBuilder::new(
        Kind::TextNote,
        format!("publish-queue-representation-benchmark-{intent:08}"),
    )
    .custom_created_at(Timestamp::from(1_000_000 + intent as u64))
    .sign_with_keys(keys)
    .expect("sign benchmark event")
}

fn frozen_event(signed: &Event) -> Event {
    Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        sentinel_signature(),
    )
}

fn relay(index: usize) -> RelayUrl {
    RelayUrl::parse(&format!("wss://delivery-{index:04}.bench.invalid")).expect("benchmark relay")
}

fn semantic_snapshot(store: &RedbStore) -> (usize, usize, String) {
    let intents = store.recover_publish_queue().expect("recover delivery");
    let mut normalized = String::new();
    let mut lanes = 0usize;
    for intent in &intents {
        writeln!(
            normalized,
            "intent:{}:{}:{}:{:?}:{}",
            intent.intent_id.0,
            intent.receipt_id,
            intent.frozen.id,
            intent.sig_state,
            intent.accepted_at.as_secs()
        )
        .unwrap();
        let receipt = store
            .reattach_receipt(intent.receipt_id)
            .expect("receipt lookup")
            .expect("retained receipt");
        writeln!(
            normalized,
            "receipt:{}:{:?}:{:?}:{}",
            receipt.receipt_id, receipt.intent_id, receipt.state, receipt.frozen_id
        )
        .unwrap();
        let token = format!("delivery-bench-{:08}", intent.intent_id.0 - 1);
        writeln!(
            normalized,
            "correlation:{token}:{:?}",
            store
                .lookup_correlation(&token)
                .expect("correlation lookup")
        )
        .unwrap();
        for revision in store
            .recover_route_revisions(intent.intent_id)
            .expect("route revisions")
        {
            writeln!(
                normalized,
                "route:{}:{}:{:?}",
                revision.intent_id.0, revision.ordinal, revision.relays
            )
            .unwrap();
        }
        for attempt in store.recover_attempts(intent.intent_id).expect("attempts") {
            writeln!(
                normalized,
                "attempt:{}:{}:{}:{}:{:?}",
                attempt.intent_id.0,
                attempt.relay,
                attempt.ordinal,
                attempt.event.id,
                attempt.outcome
            )
            .unwrap();
        }
        for detail in store
            .recover_attempt_details(intent.intent_id)
            .expect("attempt details")
        {
            let handoff = detail
                .handoff
                .as_ref()
                .map(|handoff| (handoff.at.as_secs(), format!("{:?}", handoff.result)));
            let transient = detail.transient.as_ref().map(|transient| {
                (
                    transient.eligible_at.as_secs(),
                    format!("{:?}", transient.cause),
                    transient.raw_reason.as_deref(),
                )
            });
            writeln!(
                normalized,
                "detail:{}:{}:{}:{:?}:{:?}:{:?}:{:?}",
                detail.intent_id.0,
                detail.relay,
                detail.ordinal,
                handoff,
                transient,
                detail.finished_at,
                detail.terminal
            )
            .unwrap();
        }
        for lane in store
            .recover_publish_queue_lanes(intent.intent_id)
            .expect("delivery lanes")
        {
            lanes += 1;
            writeln!(
                normalized,
                "lane:{}:{}:{}:{}:{:?}",
                lane.key.intent_id.0, lane.key.relay, lane.revision, lane.last_ordinal, lane.state
            )
            .unwrap();
        }
    }
    for deadline in store
        .due_publish_queue_deadlines(Timestamp::from(u64::MAX), 1_024)
        .expect("delivery deadlines")
    {
        writeln!(
            normalized,
            "deadline:{}:{}:{}:{}:{:?}",
            deadline.key.intent_id.0,
            deadline.key.relay,
            deadline.at.as_secs(),
            deadline.lane_revision,
            deadline.kind
        )
        .unwrap();
    }
    let digest = blake3::hash(normalized.as_bytes()).to_hex().to_string();
    (lanes, normalized.len(), digest)
}

fn populate(path: &Path, intents: usize, relays_per_intent: usize) -> BenchResult {
    assert!(intents > 0 && relays_per_intent > 0);
    let keys = fixed_keys();
    let before = Counters::sample();
    let started = Instant::now();
    let mut store = RedbStore::open(path).expect("open benchmark store");
    for intent_index in 0..intents {
        let signed = signed_event(&keys, intent_index);
        let accepted = store
            .accept_write(AcceptWrite {
                frozen: frozen_event(&signed),
                replaceable_base: None,
                monotonic_stamp: false,
                expected_pubkey: keys.public_key(),
                signing_identity_ref: "delivery-benchmark".into(),
                durability: WriteDurability::Durable,
                routing: "fixed-representative-fixture".into(),
                sig_state: IntentSigState::Pending,
                accepted_at: Timestamp::from(2_000_000 + intent_index as u64),
                correlation: Some(
                    CorrelationToken::try_from(
                        format!("delivery-bench-{intent_index:08}").as_str(),
                    )
                    .expect("bounded correlation"),
                ),
            })
            .expect("accept benchmark write");
        let intent_id = accepted.journaled_intent_id().expect("accepted intent");
        store
            .promote_signed(intent_id, signed.sig)
            .expect("promote benchmark write");
        store
            .record_route_revision(intent_id, (0..relays_per_intent).map(relay).collect())
            .expect("record benchmark route");
        let seeded = store
            .bootstrap_publish_queue_lanes(intent_id)
            .expect("bootstrap benchmark lanes");
        for lane in seeded {
            let eligible = store
                .set_lane_eligible(
                    &lane.key,
                    lane.revision,
                    Timestamp::from(3_000_000 + intent_index as u64),
                )
                .expect("eligible benchmark lane");
            let (attempt, in_flight) = store
                .start_lane_attempt(
                    &lane.key,
                    eligible.revision,
                    signed.clone(),
                    Timestamp::from(3_100_000 + intent_index as u64),
                )
                .expect("start benchmark attempt");
            store
                .record_lane_handoff(
                    &lane.key,
                    in_flight.revision,
                    attempt.ordinal,
                    PublishQueueAttemptHandoff {
                        at: Timestamp::from(3_200_000 + intent_index as u64),
                        result: HandoffEvidence::Ambiguous,
                    },
                    PublishQueuePostHandoffState::Transient {
                        eligible_at: Timestamp::from(4_000_000 + intent_index as u64),
                        cause: nmp_store::PublishQueueTransientCause::ConnectionLost,
                        raw_reason: Some("fixed representative transient".into()),
                    },
                )
                .expect("record benchmark handoff");
        }
    }
    drop(store);
    let wall_ns = elapsed_ns(started);
    let counters = Counters::sample().delta(before);

    let reopened = RedbStore::open(path).expect("settle benchmark store");
    let (lanes, normalized_semantic_bytes, scheduler_effect_digest) = semantic_snapshot(&reopened);
    drop(reopened);
    let (database_logical_bytes, database_allocated_bytes) = database_bytes(path);
    BenchResult {
        schema: "nmp-publish-queue-representation-v1",
        phase: "populate",
        intents,
        relays_per_intent,
        lanes,
        expected_commits: intents.saturating_mul(4 + 3 * relays_per_intent),
        wall_ns,
        allocation_ops: counters.allocation_ops,
        allocated_bytes: counters.allocated_bytes,
        process_write_bytes: counters.process_write_bytes,
        database_logical_bytes,
        database_allocated_bytes,
        normalized_semantic_bytes,
        scheduler_effect_digest,
    }
}

fn recover(path: &Path, intents: usize, relays_per_intent: usize) -> BenchResult {
    let before = Counters::sample();
    let started = Instant::now();
    let store = RedbStore::open(path).expect("reopen benchmark store");
    let (lanes, normalized_semantic_bytes, scheduler_effect_digest) = semantic_snapshot(&store);
    let wall_ns = elapsed_ns(started);
    let counters = Counters::sample().delta(before);
    assert_eq!(lanes, intents.saturating_mul(relays_per_intent));
    let (database_logical_bytes, database_allocated_bytes) = database_bytes(path);
    BenchResult {
        schema: "nmp-publish-queue-representation-v1",
        phase: "recover",
        intents,
        relays_per_intent,
        lanes,
        expected_commits: intents.saturating_mul(4 + 3 * relays_per_intent),
        wall_ns,
        allocation_ops: counters.allocation_ops,
        allocated_bytes: counters.allocated_bytes,
        process_write_bytes: counters.process_write_bytes,
        database_logical_bytes,
        database_allocated_bytes,
        normalized_semantic_bytes,
        scheduler_effect_digest,
    }
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let phase = args.next().expect("phase: populate or recover");
    let path = args.next().expect("database path");
    let intents = args
        .next()
        .expect("intent count")
        .to_string_lossy()
        .parse()
        .expect("numeric intent count");
    let relays = args
        .next()
        .expect("relay count")
        .to_string_lossy()
        .parse()
        .expect("numeric relay count");
    assert!(args.next().is_none(), "unexpected trailing arguments");
    let path = Path::new(&path);
    let result = match phase.to_string_lossy().as_ref() {
        "populate" => populate(path, intents, relays),
        "recover" => recover(path, intents, relays),
        other => panic!("unknown phase {other}"),
    };
    println!("{}", serde_json::to_string(&result).unwrap());
}
