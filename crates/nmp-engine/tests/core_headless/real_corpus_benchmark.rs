use super::*;

/// Reproducible real-corpus resolver/store handoff matrix for issue #168.
///
/// Transport parsing and verification have their own checked harness in
/// `nmp-transport`; this measures the next stage from an already typed,
/// verified relay batch through governed resolver ingest and one crash-atomic
/// redb transaction. Setup, database creation, and event cloning are outside
/// the timed interval.
#[test]
#[ignore = "requires NMP_CORPUS real-event JSONL"]
fn real_corpus_typed_batch_to_redb_matrix() {
    use std::hint::black_box;

    use nostr::{Event, JsonUtil};

    let path = std::env::var("NMP_CORPUS").expect("set NMP_CORPUS to event JSONL");
    let source = std::fs::read_to_string(&path).expect("read real corpus");
    let corpus: Vec<Event> = source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Event::from_json(line).expect("parse real event fixture"))
        .collect();
    assert!(!corpus.is_empty(), "real corpus is empty");

    fn median(mut samples: Vec<Duration>) -> Duration {
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    let relay = RelayUrl::parse("wss://real-corpus-bench.invalid").unwrap();
    let handle = RelayHandle {
        slot: 0,
        generation: 1,
    };
    let session = public_session(&relay);
    println!("corpus={path}");
    println!("corpus_events={}", corpus.len());
    for requested in [1usize, 2, 8, 32, 128, 512, corpus.len()] {
        let size = requested.min(corpus.len());
        let mut samples = Vec::new();
        for _ in 0..3 {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = RedbStore::open(dir.path().join("bench.redb")).expect("open redb");
            let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
            let _ = core.handle(EngineMsg::RelayConnected(handle, session.clone()));
            let frames: Vec<_> = corpus[..size]
                .iter()
                .cloned()
                .map(|event| {
                    (
                        handle,
                        std::sync::Arc::new(session.clone()),
                        RelayFrame::from(RelayMessage::event(
                            SubscriptionId::new("nmp-bench"),
                            event,
                        )),
                    )
                })
                .collect();

            let started = Instant::now();
            black_box(core.handle(EngineMsg::RelayFrames(frames)));
            samples.push(started.elapsed());
        }
        println!("size={size}");
        println!(
            "  typed_resolver_redb_median_ms={:.3}",
            median(samples).as_secs_f64() * 1_000.0
        );
    }
}
