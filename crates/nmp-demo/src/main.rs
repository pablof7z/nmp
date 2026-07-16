//! `nmp-demo` — the Rust sibling of the iOS falsifier (M5): a CLI that
//! proves the NMP engine works end-to-end against REAL public relays, from
//! an application's point of view.
//!
//! Flow:
//! 1. Parse an npub/hex pubkey (default: a well-known active npub) and
//!    optional `--nsec`/`--secs`.
//! 2. Construct `nmp::Engine::new` with ONLY the two hardcoded operator
//!    indexer relays configured -- no write relay pre-resolved for anyone.
//!    Set the active account to `target` and `observe` the $myFollows
//!    `LiveQuery` (kind:1 authored by whoever the target's kind:3 currently
//!    names, reactively).
//! 3. The ENGINE ITSELF (M5's self-bootstrapping outbox --
//!    `nmp_engine::core::EngineCore`'s internal kind:10002 auto-discovery,
//!    reached here only through the `nmp` facade) notices the target -- and,
//!    as its kind:3 resolves, every follow -- has no known write relays yet,
//!    opens its OWN discovery reads against the two indexers, and re-routes
//!    each author's kind:1 atom to their real write relay the moment that
//!    author's relay list arrives. This app never resolves a single relay
//!    itself, and never touches a mechanism crate (`nmp-store`/`nmp-router`/
//!    `nmp-transport`/`nmp-resolver`) directly: it only configures the two
//!    indexers through `nmp::EngineConfig` and subscribes (no bootstrap
//!    phase, no pre-resolution -- see `docs/known-gaps.md`'s former
//!    "RelayDirectory" gap).
//! 4. Print every row as it streams in, plus whatever diagnostic the facade
//!    surface actually exposes (see the running summary for what that is and
//!    is not).
//! 5. Stop after `--secs` (default 20), print a summary, shut down clean.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nmp::{
    Binding, Demand, Derived, DiagnosticsSnapshot, Durability, Engine, EngineConfig, Filter, Frame,
    IdentityField, Kind, LiveQuery, PublicKey, RowDelta, Selector, Timestamp, UnsignedEvent,
    WriteIntent, WritePayload, WriteRouting,
};
use nostr::Keys;

/// fiatjaf -- a well-known, consistently-active npub with many follows, so
/// a read-only run against it reliably has live data to show.
const DEFAULT_NPUB: &str = "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6";

/// The two operator indexer relays this demo configures `nmp::EngineConfig`
/// with -- the entire relay fact set it ever supplies. Every author's write
/// relays, including the target's own, are discovered live by the engine
/// from here on (see the module doc).
const INDEXER_RELAYS: [&str; 2] = ["wss://purplepag.es", "wss://relay.primal.net"];

struct Args {
    pubkey: String,
    nsec: Option<String>,
    secs: u64,
}

fn parse_args() -> Args {
    parse_args_from(std::env::args().skip(1))
}

/// Testable core of [`parse_args`]: takes an explicit argv (sans program
/// name) rather than reading `std::env::args()` directly.
fn parse_args_from(argv: impl Iterator<Item = String>) -> Args {
    let mut pubkey = DEFAULT_NPUB.to_string();
    let mut nsec = None;
    let mut secs = 20u64;
    let mut positional_seen = false;

    let mut it = argv;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--nsec" => nsec = it.next(),
            "--secs" => {
                if let Some(v) = it.next() {
                    secs = v.parse().unwrap_or(secs);
                }
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other if !positional_seen => {
                pubkey = other.to_string();
                positional_seen = true;
            }
            other => {
                eprintln!("nmp-demo: unrecognized argument {other:?}");
                print_usage();
                std::process::exit(2);
            }
        }
    }

    Args { pubkey, nsec, secs }
}

fn print_usage() {
    eprintln!(
        "usage: nmp-demo [npub|hex] [--nsec <nsec>] [--secs <seconds>]\n\
         \n\
         Subscribes to the follow-feed (kind:1 authored by whoever the\n\
         given pubkey's kind:3 contact list currently names) via the NMP\n\
         engine against real relays. The engine self-navigates outbox\n\
         routing from two operator indexer relays alone (wss://purplepag.es,\n\
         wss://relay.primal.net) -- this app never resolves a relay itself.\n\
         Read-only unless --nsec is given. Runs for --secs then exits."
    );
}

fn main() {
    let args = parse_args();

    let target = match PublicKey::parse(&args.pubkey) {
        Ok(pk) => pk,
        Err(e) => {
            eprintln!(
                "nmp-demo: could not parse {:?} as an npub/hex pubkey: {e}",
                args.pubkey
            );
            std::process::exit(2);
        }
    };

    println!("nmp-demo -- NMP engine end-to-end falsifier (Rust CLI)");
    println!("target pubkey : {}", target.to_hex());
    println!("run duration  : {}s", args.secs);
    println!("indexer relays: {}", INDEXER_RELAYS.join(", "));

    // No bootstrap phase: `Engine::new` starts knowing NOTHING beyond the
    // indexer set below. Every author's write relays -- including the
    // target's own -- are discovered by the engine itself, live, from here
    // on (M5's self-bootstrapping outbox, reached only through the facade
    // now -- see the module doc).
    let engine = match Engine::new(EngineConfig {
        indexer_relays: INDEXER_RELAYS.iter().map(|u| u.to_string()).collect(),
        ..EngineConfig::default()
    }) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("nmp-demo: could not construct the engine: {e}");
            std::process::exit(2);
        }
    };
    println!(
        "\n-- no bootstrap phase: the engine discovers write relays live from the \
         indexers above as demand needs them --"
    );

    match &args.nsec {
        Some(nsec) => match engine.add_account(nsec) {
            Ok(account) => println!(
                "signer: loaded from --nsec ({})",
                account.public_key().to_hex()
            ),
            Err(e) => {
                eprintln!("nmp-demo: --nsec did not parse as a valid secret key: {e}");
                std::process::exit(2);
            }
        },
        None => {
            println!("signer: ephemeral (read-only run; pass --nsec to also demo publish)");
        }
    }

    // Read-side identity is the TARGET we're viewing. M4 §5 couples the read
    // root and the active signing capability behind ONE verb
    // (`set_active_account`) so a real account switch can never leave them
    // pointing at different accounts -- but browsing a target you hold no
    // key for is still legal: if `--nsec`'s own pubkey differs from
    // `target`, the registry simply has no signer registered under
    // `target`, so any publish attempted while viewing it terminates
    // `WriteStatus::Failed` (no active signer) rather than silently signing
    // under the wrong key.
    engine
        .set_active_account(Some(target))
        .expect("engine is open just after construction");

    let my_follows = build_follow_feed_query();

    println!("\n-- subscribing to the follow-feed (kind:1 by target's kind:3 contacts) --\n");
    let subscription = engine
        .observe(my_follows, None)
        .expect("engine is open just after construction");

    // `Subscription::recv()` is a blocking call -- the facade has no
    // `recv_timeout` (unlike the raw `Handle`'s row channel this app drove
    // directly before #52). Forward it onto its own `mpsc` channel from a
    // dedicated thread, the same pattern the diagnostics drain below already
    // uses, so `--secs` can still bound how long THIS app waits without
    // blocking on a facade primitive that has no notion of a deadline
    // itself.
    let (rows_tx, rows_rx) = mpsc::channel::<Frame>();
    // `_rows_thread`: not joined, for the same reason `_diag_thread` below
    // isn't (see the note at the end of `main`) -- this process reads
    // `rows_rx` directly and exits; the OS reclaims the detached thread (and
    // the `Subscription` it owns, whose `Drop` unsubscribes) on exit.
    let _rows_thread = thread::spawn(move || {
        while let Ok(msg) = subscription.recv() {
            if rows_tx.send(msg).is_err() {
                break; // main thread stopped listening; nothing left to forward
            }
        }
    });

    // Live diagnostics (M5 plan §1): the engine-owned, read-only surface
    // exposing exactly what's on the wire -- per-relay wire-sub count, the
    // EXACT filter JSON currently sent, and events actually received per
    // (relay, kind). A dedicated thread drains `observe_diagnostics`'s
    // "latest value wins" stream (never a poll loop, D8) into a shared
    // slot this app reads from once the timed run ends, so the final
    // printed snapshot reflects the run's steady state, not just whatever
    // happened to be current at subscribe time.
    let diagnostics = engine
        .observe_diagnostics()
        .expect("engine is open just after construction");
    let latest_diag: Arc<Mutex<Option<DiagnosticsSnapshot>>> = Arc::new(Mutex::new(None));
    let latest_diag_writer = Arc::clone(&latest_diag);
    // `_diag_thread`: not joined -- this process reads `latest_diag`'s
    // current value directly and exits (see the note at the end of `main`),
    // so there is nothing to wait on here either.
    let _diag_thread = thread::spawn(move || {
        while let Some(snapshot) = diagnostics.recv() {
            *latest_diag_writer
                .lock()
                .expect("diag snapshot mutex poisoned") = Some(snapshot);
        }
    });

    // Optional publish demo: only if the caller gave us a real key to sign
    // with AND that key's own pubkey (so the OK/receipt has somewhere real
    // to route via outbox -- this demo does not fabricate a contact list
    // for an ephemeral key).
    let mut receipt_rx = None;
    if let Some(pk) = signer_pubkey_if_real(&args.nsec) {
        let content = format!("nmp-demo end-to-end falsifier run @ {}", Timestamp::now());
        let unsigned = UnsignedEvent::new(pk, Timestamp::now(), Kind::TextNote, vec![], content);
        println!("-- publishing a demo text note as --nsec's own pubkey --");
        receipt_rx = Some(
            engine
                .publish(WriteIntent {
                    payload: WritePayload::Unsigned(unsigned),
                    durability: Durability::Durable,
                    routing: WriteRouting::AuthorOutbox,
                    identity_override: None,
                })
                .expect("engine is open just after construction"),
        );
    }

    let deadline = Instant::now() + Duration::from_secs(args.secs);

    // `Handle::subscribe`'s `RowsMsg` batch is an INCREMENTAL delta (plan
    // fix for the P0 redelivery blow-up in `docs/known-gaps.md`): each
    // batch carries only the rows ADDED and REMOVED since this handle's
    // last batch, never the query's full current row set. This app owns
    // the accumulation into its own live row set (`known_notes`) -- exactly
    // what M4's Swift bridge will do into a snapshot array, so
    // `AsyncSequence<[Row]>` ergonomics still hold even though the WIRE is
    // deltas. `raw_delta_entries` counts every `Added`/`Removed` entry ever
    // delivered across the whole run -- with the fix, this should track the
    // distinct-note count (each note delivered ~once), not blow up
    // quadratically as the feed grows.
    let mut known_notes: BTreeMap<nmp::EventId, nmp::Event> = BTreeMap::new();
    let mut raw_delta_entries = 0usize;
    let mut total_batches = 0usize;
    let mut kind_counts: BTreeMap<u16, usize> = BTreeMap::new();
    let mut authors_seen: BTreeSet<String> = BTreeSet::new();
    let mut last_coverage_printed: Option<String> = None;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rows_rx.recv_timeout(remaining) {
            Ok(frame) => {
                let deltas = frame.deltas;
                let coverage = frame.evidence;
                total_batches += 1;
                raw_delta_entries += deltas.len();
                let coverage_str = format!("{coverage:?}");
                for delta in deltas {
                    match delta {
                        RowDelta::Added(row) => {
                            let event = row.event;
                            if known_notes.insert(event.id, event.clone()).is_some() {
                                continue; // already rendered this exact event; skip re-printing it
                            }
                            *kind_counts.entry(event.kind.as_u16()).or_default() += 1;
                            authors_seen.insert(event.pubkey.to_hex());
                            let preview: String = event.content.chars().take(80).collect();
                            println!(
                                "[note] author={} created_at={} \"{}\" (sources: {})",
                                event.pubkey.to_hex(),
                                event.created_at.as_secs(),
                                preview.replace('\n', " "),
                                row.sources.len(),
                            );
                        }
                        RowDelta::SourcesGrew { id, sources } => {
                            if known_notes.contains_key(&id) {
                                println!(
                                    "[sources] {} now confirmed by {} relay(s)",
                                    id,
                                    sources.len()
                                );
                            }
                        }
                        RowDelta::Removed(id) => {
                            known_notes.remove(&id);
                        }
                    }
                }
                // Every fresh subscribe delivers one (possibly empty) batch;
                // coverage on its own is still worth surfacing once it
                // changes, since it's the only aggregate liveness signal
                // this Subscription currently exposes (see summary).
                if last_coverage_printed.as_deref() != Some(coverage_str.as_str()) {
                    println!("[coverage] {coverage_str}");
                    last_coverage_printed = Some(coverage_str);
                }
            }
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => {
                println!("engine dropped the row channel (unexpected) -- stopping early");
                break;
            }
        }
    }
    let total_rows = known_notes.len();

    if let Some(rx) = receipt_rx {
        // Drain whatever receipt status arrived without extending the
        // bounded run -- best-effort, never blocks past the deadline.
        while let Ok(status) = rx.try_recv() {
            println!("[receipt] {status:?}");
        }
    }

    println!("\n-- summary --");
    println!("distinct kind:1 notes rendered : {total_rows}");
    println!("distinct authors seen          : {}", authors_seen.len());
    println!("rows by kind (distinct)        : {kind_counts:?}");
    println!(
        "row batches delivered on Engine::observe's channel : {total_batches} \
         (raw Added+Removed delta-entry count across all batches, now that \
         `EmitRows` delivers incremental deltas rather than the full \
         current row set on every refresh: {raw_delta_entries})"
    );
    let delta_ratio = raw_delta_entries as f64 / total_rows.max(1) as f64;
    if total_batches > 0 && raw_delta_entries > total_rows.max(1) * 3 {
        println!(
            "WARNING: raw delta-entry volume is {delta_ratio:.1}x the distinct note count -- \
             deltas are being re-delivered far more than once per row (expected ~1x). This \
             would mean the P0 redelivery blow-up (docs/known-gaps.md) has regressed."
        );
    } else if total_batches > 0 {
        println!(
            "raw delta-entry volume is {delta_ratio:.1}x the distinct note count (expected \
             ~1x with incremental delivery -- was 635-1294x before the fix)."
        );
    }
    // Read whatever diagnostics snapshot the drain thread has captured so
    // far -- deliberately BEFORE shutting down. `--secs` bounds how long
    // this app waits for ROWS, not how long the engine thread takes to
    // finish draining its own inbox of already-in-flight relay frames (a
    // popular account's backlog can keep the engine busy well past the
    // nominal deadline -- a separate, pre-existing per-event recompile cost,
    // NOT the kind:10002 discovery-churn bug this snapshot is here to
    // falsify). Reading the "latest value wins" slot directly reports the
    // ground truth as of NOW rather than blocking this report on that
    // unrelated drain.
    let final_diag = latest_diag
        .lock()
        .expect("diag snapshot mutex poisoned")
        .clone();
    println!("\n-- diagnostics (snapshot as of the --secs deadline) --");
    match final_diag {
        Some(snapshot) => {
            println!("relays                  : {}", snapshot.relays.len());
            println!(
                "uncovered authors        : {}",
                snapshot.uncovered_author_count
            );
            if !snapshot.dropped_merge_rules.is_empty() {
                println!(
                    "dropped merge rules      : {:?}",
                    snapshot.dropped_merge_rules
                );
            }
            for relay in &snapshot.relays {
                println!("\nrelay: {}", relay.relay);
                println!("  wire_sub_count  : {}", relay.wire_sub_count);
                println!("  authors_served  : {}", relay.authors_served);
                println!("  by_lane         : {:?}", relay.by_lane);
                println!("  exact filters   :");
                for f in &relay.filters {
                    println!("    {f}");
                }
                println!("  events_by_kind  : {:?}", relay.events_by_kind);
                if let Some((_, kind10002_count)) =
                    relay.events_by_kind.iter().find(|(k, _)| *k == 10_002)
                {
                    let ratio = *kind10002_count as f64 / relay.authors_served.max(1) as f64;
                    println!(
                        "  kind:10002 events={kind10002_count} vs authors_served={} (ratio {ratio:.1}x)",
                        relay.authors_served
                    );
                    if ratio > 3.0 {
                        println!(
                            "  WARNING: kind:10002 event volume is {ratio:.1}x the resolved \
                             author count -- see docs/known-gaps.md's discovery over-fetch \
                             finding."
                        );
                    }
                }
            }
        }
        None => println!("no diagnostics snapshot was ever received during this run"),
    }

    // Best-effort teardown: `Engine::shutdown` is enqueued behind whatever
    // the engine thread's own inbox still has backlogged (see the note above
    // -- a popular account's already-in-flight relay frames can take a while
    // to fully drain). The report above is this run's actual deliverable;
    // once it's printed there is nothing further this process needs to wait
    // on, so it exits directly once `shutdown` returns rather than joining
    // the detached `_rows_thread`/`_diag_thread` (their `Subscription`/
    // `DiagnosticsSubscription` disconnect and drop cleanly once `shutdown`
    // tears the engine thread down -- see `Engine`'s own doc).
    engine.shutdown();
    std::process::exit(0);
}

/// Only treat the signer as "real" (worth using as a publish author) if the
/// caller actually supplied `--nsec`; the ephemeral fallback key has no
/// contact list / outbox facts this demo resolved for it, so publishing
/// under it would just be routed nowhere.
fn signer_pubkey_if_real(nsec: &Option<String>) -> Option<PublicKey> {
    nsec.as_ref()
        .and_then(|s| Keys::parse(s).ok())
        .map(|k| k.public_key())
}

/// The `$myFollows` shape (VISION-grammar terms): kind:1 authored by
/// whoever the active pubkey's kind:3 contact list currently names
/// (projected through the `p` tag) -- identical shape to
/// `nmp-engine`'s own `runtime_integration.rs` test.
fn build_follow_feed_query() -> LiveQuery {
    LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            }),
            project: Selector::Tag("p".to_string()),
        }))),
        ..Filter::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_defaults_to_the_well_known_npub_and_20_seconds() {
        let args = parse_args_from(std::iter::empty());
        assert_eq!(args.pubkey, DEFAULT_NPUB);
        assert_eq!(args.secs, 20);
        assert!(args.nsec.is_none());
    }

    #[test]
    fn parse_args_reads_positional_pubkey_and_flags() {
        let argv = [
            "deadbeef".to_string(),
            "--secs".to_string(),
            "5".to_string(),
        ];
        let args = parse_args_from(argv.into_iter());
        assert_eq!(args.pubkey, "deadbeef");
        assert_eq!(args.secs, 5);
    }

    #[test]
    fn parse_args_reads_nsec_flag() {
        let argv = ["--nsec".to_string(), "nsec1abc".to_string()];
        let args = parse_args_from(argv.into_iter());
        assert_eq!(args.nsec.as_deref(), Some("nsec1abc"));
        assert_eq!(
            args.pubkey, DEFAULT_NPUB,
            "unset positional keeps the default"
        );
    }

    #[test]
    fn default_npub_parses_as_a_valid_pubkey() {
        // No network: just verifies the compiled-in default is well-formed
        // bech32, so a bare `nmp-demo` invocation never fails at the
        // pubkey-parsing step before it even gets to bootstrap.
        PublicKey::parse(DEFAULT_NPUB).expect("DEFAULT_NPUB must parse");
    }

    #[test]
    fn follow_feed_query_is_kind1_derived_from_active_pubkeys_kind3_contacts() {
        let LiveQuery(demand) = build_follow_feed_query();
        let filter = demand.selection;
        assert_eq!(filter.kinds, Some(BTreeSet::from([1u16])));
        assert_eq!(demand.source, nmp::SourceAuthority::AuthorOutboxes);
        match filter.authors {
            Some(Binding::Derived(derived)) => {
                assert_eq!(derived.inner.selection.kinds, Some(BTreeSet::from([3u16])));
                assert_eq!(
                    derived.inner.selection.authors,
                    Some(Binding::Reactive(IdentityField::ActivePubkey))
                );
                assert_eq!(derived.project, Selector::Tag("p".to_string()));
            }
            other => panic!("expected a Derived authors binding, got {other:?}"),
        }
    }
}
