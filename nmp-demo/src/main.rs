//! `nmp-demo` — the Rust sibling of the iOS falsifier (M5): a CLI that
//! proves the NMP engine works end-to-end against REAL public relays, from
//! an application's point of view.
//!
//! Flow:
//! 1. Parse an npub/hex pubkey (default: a well-known active npub) and
//!    optional `--nsec`/`--secs`.
//! 2. Bootstrap (`bootstrap.rs`): resolve the target's kind:3 contacts and
//!    every follow's kind:10002 write relays from two hardcoded operator
//!    indexer relays -- entirely BEFORE the engine exists, because
//!    `RelayDirectory` is a one-shot snapshot boxed into `EngineCore` at
//!    construction (see `directory.rs`'s doc for why).
//! 3. Spawn `EngineThread`, `set_active_pubkey(target)`, and `subscribe` the
//!    $myFollows LiveQuery (kind:1 authored by whoever the target's kind:3
//!    currently names, reactively).
//! 4. Print every row as it streams in, plus whatever diagnostic the
//!    `Handle` surface actually exposes (see the running summary for what
//!    that is and is not).
//! 5. Stop after `--secs` (default 20), print a summary, shut down clean.

mod bootstrap;
mod directory;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use nmp_engine::outbox::{Durability, WriteIntent, WritePayload, WriteRouting};
use nmp_engine::runtime::EngineThread;
use nmp_grammar::{Binding, Derived, Filter, IdentityField, Selector, TagName};
use nmp_resolver::LiveQuery;
use nmp_signer::LocalKeySigner;
use nmp_store::MemoryStore;
use nmp_transport::PoolConfig;
use nostr::{Keys, PublicKey, RelayUrl};

/// fiatjaf -- a well-known, consistently-active npub with many follows, so
/// a read-only run against it reliably has live data to show.
const DEFAULT_NPUB: &str = "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6";

/// The router's per-atom relay-coverage cap (`nmp_router::Router::compile`'s
/// `cap` param) -- mirrors the value `nmp-engine`'s own runtime integration
/// test uses; not an app-tunable in this demo.
const ROUTER_CAP: usize = 10;

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
         engine against real relays, bootstrapped from two operator\n\
         indexer relays (wss://purplepag.es, wss://relay.nostr.band).\n\
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

    let indexers: Vec<RelayUrl> = ["wss://purplepag.es", "wss://relay.nostr.band"]
        .iter()
        .map(|u| RelayUrl::parse(u).expect("hardcoded indexer URL must parse"))
        .collect();
    println!(
        "indexer relays: {}",
        indexers
            .iter()
            .map(RelayUrl::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );

    println!("\n-- bootstrap: resolving contacts + NIP-65 write relays from indexers --");
    let (directory, boot_stats) = bootstrap::bootstrap(&indexers, target, Duration::from_secs(8));
    println!(
        "contact list found: {} | follows discovered: {} | kind:10002 events seen: {} \
         | authors with write relays: {} | total write-relay URLs: {} | bootstrap took {:.1}s",
        boot_stats.contact_list_found,
        boot_stats.follows_discovered,
        boot_stats.relay_list_events_seen,
        boot_stats.authors_with_write_relays,
        boot_stats.total_write_relay_urls,
        boot_stats.elapsed.as_secs_f64(),
    );
    if !boot_stats.contact_list_found {
        println!(
            "WARNING: no kind:3 contact list found for this pubkey on the indexer relays \
             within the bootstrap window -- the follow-feed will likely stay empty."
        );
    }

    let signer = match &args.nsec {
        Some(nsec) => match Keys::parse(nsec) {
            Ok(keys) => {
                println!(
                    "signer: loaded from --nsec ({})",
                    keys.public_key().to_hex()
                );
                LocalKeySigner::new(keys)
            }
            Err(e) => {
                eprintln!("nmp-demo: --nsec did not parse as a valid secret key: {e}");
                std::process::exit(2);
            }
        },
        None => {
            println!("signer: ephemeral (read-only run; pass --nsec to also demo publish)");
            LocalKeySigner::generate()
        }
    };

    let (engine_thread, handle) = EngineThread::spawn(
        MemoryStore::new(),
        directory,
        ROUTER_CAP,
        PoolConfig::default(),
        signer,
    );

    // Read-side identity is the TARGET we're viewing, independent of the
    // signer's own key (sign/publish is orthogonal to which feed we watch
    // -- see nmp-signer::LocalKeySigner's doc: the signer only checks that
    // an unsigned template's pubkey matches ITS key at Publish time, never
    // at SetActivePubkey time).
    handle.set_active_pubkey(Some(target));

    let my_follows = build_follow_feed_query();

    println!("\n-- subscribing to the follow-feed (kind:1 by target's kind:3 contacts) --\n");
    let (_query_handle, rows_rx) = handle.subscribe(my_follows);

    // Optional publish demo: only if the caller gave us a real key to sign
    // with AND that key's own pubkey (so the OK/receipt has somewhere real
    // to route via outbox -- this demo does not fabricate a contact list
    // for an ephemeral key).
    let mut receipt_rx = None;
    if let Some(pk) = signer_pubkey_if_real(&args.nsec) {
        let content = format!(
            "nmp-demo end-to-end falsifier run @ {}",
            nostr::Timestamp::now()
        );
        let unsigned = nostr::UnsignedEvent::new(
            pk,
            nostr::Timestamp::now(),
            nostr::Kind::TextNote,
            vec![],
            content,
        );
        println!("-- publishing a demo text note as --nsec's own pubkey --");
        receipt_rx = Some(handle.publish(WriteIntent {
            payload: WritePayload::Unsigned(unsigned),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        }));
    }

    let deadline = Instant::now() + Duration::from_secs(args.secs);

    // NOTE on the counters below: `Handle::subscribe`'s `RowsMsg` batch is
    // NOT an incremental delta despite the `RowDelta` name -- every
    // delivery is the query's FULL current matching row set
    // (`nmp_engine::core::EngineCore::rows_and_coverage_for` recomputes the
    // whole set from the store on every refresh; `refresh_handle` is only
    // SUPPOSED to suppress a redelivery when the row-id set and coverage
    // are both byte-for-byte unchanged since the last one). This app dedups
    // by event id before printing, as any real app must -- but it also
    // counts the raw, undeduped delivery volume, because during this run
    // that suppression did not seem to be holding: see the summary for how
    // large the gap between "distinct notes" and "raw batch deliveries" is.
    let mut total_rows = 0usize;
    let mut raw_row_deliveries = 0usize;
    let mut total_batches = 0usize;
    let mut kind_counts: BTreeMap<u16, usize> = BTreeMap::new();
    let mut authors_seen: BTreeSet<String> = BTreeSet::new();
    let mut seen_note_ids: BTreeSet<nostr::EventId> = BTreeSet::new();
    let mut last_coverage_printed: Option<String> = None;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rows_rx.recv_timeout(remaining) {
            Ok((rows, coverage)) => {
                total_batches += 1;
                raw_row_deliveries += rows.len();
                let coverage_str = format!("{coverage:?}");
                if rows.is_empty() {
                    // Every fresh subscribe delivers one such batch;
                    // coverage on its own is still worth surfacing once it
                    // changes, since it's the only aggregate liveness
                    // signal this Handle currently exposes (see summary).
                    if last_coverage_printed.as_deref() != Some(coverage_str.as_str()) {
                        println!("[coverage] {coverage_str}");
                        last_coverage_printed = Some(coverage_str);
                    }
                    continue;
                }
                for row in rows {
                    if !seen_note_ids.insert(row.event.id) {
                        continue; // already rendered this exact event; skip re-printing it
                    }
                    total_rows += 1;
                    *kind_counts.entry(row.event.kind.as_u16()).or_default() += 1;
                    authors_seen.insert(row.event.pubkey.to_hex());
                    let preview: String = row.event.content.chars().take(80).collect();
                    println!(
                        "[note] author={} created_at={} \"{}\"",
                        row.event.pubkey.to_hex(),
                        row.event.created_at.as_secs(),
                        preview.replace('\n', " "),
                    );
                }
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
        "row batches delivered on Handle::subscribe's channel : {total_batches} \
         (raw row count across all batches, before this app's own \
         dedup-by-event-id: {raw_row_deliveries})"
    );
    if total_batches > 0 && raw_row_deliveries > total_rows.max(1) * 3 {
        println!(
            "WARNING: raw delivery volume is {:.1}x the distinct note count -- the \
             SAME already-known rows are being redelivered on this Handle far more \
             than once. Not something this app can fix from the outside (no \
             delta/only-new-rows option exists on Handle::subscribe today); see the \
             run report.",
            raw_row_deliveries as f64 / total_rows.max(1) as f64
        );
    }
    println!(
        "per-relay / per-relay-kind diagnostic: NOT AVAILABLE from Handle -- \
         nmp_engine::core::RowDelta carries only the raw `nostr::Event` \
         (no relay provenance), and Handle exposes no relay-connection or \
         per-relay-count accessor at all (PoolEvent::Health is dropped \
         before it ever reaches EngineMsg). See the run report for what a \
         Handle accessor would need to expose."
    );

    handle.shutdown();
    engine_thread.join();
    println!("\nengine shut down cleanly.");
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
    LiveQuery(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            },
            project: Selector::Tag(TagName::new('p').expect("'p' is a valid tag name")),
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
        let LiveQuery(filter) = build_follow_feed_query();
        assert_eq!(filter.kinds, Some(BTreeSet::from([1u16])));
        match filter.authors {
            Some(Binding::Derived(derived)) => {
                assert_eq!(derived.inner.kinds, Some(BTreeSet::from([3u16])));
                assert_eq!(
                    derived.inner.authors,
                    Some(Binding::Reactive(IdentityField::ActivePubkey))
                );
                assert_eq!(derived.project, Selector::Tag(TagName::new('p').unwrap()));
            }
            other => panic!("expected a Derived authors binding, got {other:?}"),
        }
    }
}
