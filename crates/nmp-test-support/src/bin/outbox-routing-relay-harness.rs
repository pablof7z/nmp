use std::path::PathBuf;
use std::time::Duration;

use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::Keys;
use serde_json::json;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let manifest = PathBuf::from(args.next().expect("manifest path"));
    let stop = PathBuf::from(args.next().expect("stop path"));
    let report = PathBuf::from(args.next().expect("report path"));
    assert!(args.next().is_none(), "unexpected argument");

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(run(manifest, stop, report));
}

async fn run(manifest: PathBuf, stop: PathBuf, report: PathBuf) {
    let author = Keys::generate();
    let indexer = ScriptedRelay::start(&RelayConfig::default()).await;
    let outbox = ScriptedRelay::start(&RelayConfig::default()).await;
    let undeclared = ScriptedRelay::start(&RelayConfig::default()).await;
    indexer
        .seed_relay_list(&author, &[outbox.url.to_string()], &[], 1_700_000_000)
        .await;

    let values = json!({
        "secret_key": author.secret_key().to_secret_hex(),
        "indexer": indexer.url.to_string(),
        "outbox": outbox.url.to_string(),
        "undeclared": undeclared.url.to_string(),
    });
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&values).expect("manifest JSON"),
    )
    .expect("write manifest");

    while !stop.exists() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let indexer_queries = indexer.query_count_for_kind(10_002);
    let expected_author = author.public_key().to_hex();
    let author_scoped_queries = indexer
        .wire_record()
        .reqs
        .into_iter()
        .filter(|request| {
            request.kinds().contains(&10_002) && request.authors().contains(&expected_author)
        })
        .count();
    let indexer_events = indexer.admitted_event_count();
    let outbox_events = outbox.admitted_event_count();
    let undeclared_contacts = undeclared.contact_count();
    let passed = indexer_queries > 0
        && author_scoped_queries > 0
        && indexer_events == 0
        && outbox_events == 1
        && undeclared_contacts == 0;
    let evidence = json!({
        "passed": passed,
        "indexer_kind_10002_queries": indexer_queries,
        "author_scoped_kind_10002_queries": author_scoped_queries,
        "indexer_events": indexer_events,
        "outbox_events": outbox_events,
        "undeclared_contacts": undeclared_contacts,
    });
    std::fs::write(
        &report,
        serde_json::to_vec_pretty(&evidence).expect("report JSON"),
    )
    .expect("write report");

    undeclared.shutdown();
    outbox.shutdown();
    indexer.shutdown();
    assert!(passed, "cold-discovery witness failed: {evidence}");
}
