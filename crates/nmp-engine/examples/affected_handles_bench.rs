use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nmp_engine::core::{EngineCore, EngineMsg, RowDelta, RowSink};
use nmp_grammar::{Binding, Filter, IndexedTagName, RelaySessionKey};
use nmp_resolver::LiveQuery;
use nmp_router::FixtureDirectory;
use nmp_store::{EventStore, RedbStore, RelayObserved};
use nmp_transport::{RelayFrame, RelayHandle};
use nostr::{
    Event, EventBuilder, JsonUtil, Keys, Kind, RelayMessage, RelayUrl, SubscriptionId, Tag,
    Timestamp,
};

struct NullSink;

impl RowSink for NullSink {
    fn on_rows(&self, _rows: Vec<RowDelta>) {}
}

fn database_path(handles: usize) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    env::temp_dir().join(format!(
        "nmp-affected-handles-{}-{handles}-{nonce}.redb",
        std::process::id()
    ))
}

fn room_query(room: &str) -> LiveQuery {
    LiveQuery::from_filter(Filter {
        tags: BTreeMap::from([(
            IndexedTagName::new('h').unwrap(),
            Binding::Literal(BTreeSet::from([room.to_owned()])),
        )]),
        limit: Some(200),
        ..Filter::default()
    })
}

fn load_corpus(path: &Path) -> (Vec<Event>, Vec<String>, u64) {
    let source = fs::read_to_string(path).expect("read real NMP JSONL corpus");
    let events: Vec<Event> = source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Event::from_json(line).expect("parse real corpus event"))
        .collect();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for event in &events {
        for tag in event.tags.iter() {
            let fields = tag.as_slice();
            if fields.first().map(String::as_str) == Some("h") {
                if let Some(room) = fields.get(1) {
                    *counts.entry(room.clone()).or_default() += 1;
                }
            }
        }
    }
    let mut rooms: Vec<_> = counts.into_iter().collect();
    rooms.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let rooms = rooms.into_iter().map(|(room, _)| room).collect();
    let max_created_at = events
        .iter()
        .map(|event| event.created_at.as_secs())
        .max()
        .unwrap_or(0);
    (events, rooms, max_created_at)
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn run_case(label: &str, events: &[Event], rooms: &[String], max_created_at: u64, handles: usize) {
    let db_path = database_path(handles);
    let relay = RelayUrl::parse("wss://real-corpus-bench.example").unwrap();
    let mut store = RedbStore::open(&db_path).unwrap();
    store
        .insert_batch(
            events
                .iter()
                .cloned()
                .map(|event| {
                    (
                        event,
                        RelayObserved::new(relay.clone(), Timestamp::from(max_created_at + 1)),
                    )
                })
                .collect(),
        )
        .unwrap();

    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
    let relay_handle = RelayHandle {
        slot: 0,
        generation: 1,
    };
    let session = RelaySessionKey::public(relay.clone());
    black_box(core.handle(EngineMsg::RelayConnected(relay_handle, session.clone())));
    for room in rooms.iter().take(handles) {
        black_box(core.handle(EngineMsg::Subscribe(room_query(room), Box::new(NullSink))));
    }

    let keys = Keys::generate();
    let busiest = rooms.first().expect("corpus must contain an h-tagged room");
    let mut samples = Vec::with_capacity(50);
    for iteration in 0..50u64 {
        let created_at = max_created_at + 10 + iteration;
        let event = EventBuilder::new(Kind::from(9u16), format!("bench-{iteration}"))
            .tag(Tag::parse(["h".to_owned(), busiest.clone()]).unwrap())
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(&keys)
            .unwrap();
        let frame = RelayFrame::from(RelayMessage::event(
            SubscriptionId::new("affected-handles-bench"),
            event,
        ));
        let started = Instant::now();
        let effects = core.handle(EngineMsg::RelayFrame(relay_handle, session.clone(), frame));
        samples.push(started.elapsed());
        black_box(effects);
    }
    let p50 = median(&mut samples);
    println!(
        "dataset={label} handles={handles} events={} p50_ingest_refresh_ms={:.3}",
        events.len(),
        p50.as_secs_f64() * 1_000.0
    );

    drop(core);
    fs::remove_file(db_path).unwrap();
}

fn synthetic_rooms() -> (Vec<Event>, Vec<String>, u64) {
    let keys = Keys::generate();
    let rooms: Vec<_> = (0..64)
        .map(|room| format!("synthetic-room-{room}"))
        .collect();
    let mut events = Vec::with_capacity(64 * 200);
    for (room_index, room) in rooms.iter().enumerate() {
        for ordinal in 0..200usize {
            let created_at = (room_index * 200 + ordinal + 1) as u64;
            events.push(
                EventBuilder::new(Kind::from(9u16), format!("seed-{room_index}-{ordinal}"))
                    .tag(Tag::parse(["h".to_owned(), room.clone()]).unwrap())
                    .custom_created_at(Timestamp::from(created_at))
                    .sign_with_keys(&keys)
                    .unwrap(),
            );
        }
    }
    (events, rooms, 64 * 200)
}

fn main() {
    let corpus = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/nmp-real-v3-import.jsonl"));
    let (events, rooms, max_created_at) = load_corpus(&corpus);
    assert!(
        rooms.len() >= 8,
        "real corpus needs at least 8 distinct h tags"
    );
    for handles in [1usize, 8, rooms.len().min(16)] {
        run_case("real", &events, &rooms, max_created_at, handles);
    }
    let (synthetic_events, synthetic_rooms, synthetic_max) = synthetic_rooms();
    for handles in [1usize, 8, 32, 64] {
        run_case(
            "synthetic-64x200",
            &synthetic_events,
            &synthetic_rooms,
            synthetic_max,
            handles,
        );
    }
}
