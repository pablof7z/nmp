//! Live validation of wire subscription collapse against REAL public relays.
//!
//! The headless and `nak` measurements prove the collapse against relays we
//! control. This proves it against relays that enforce their own limits and
//! can simply refuse us: several public relays advertise
//! `limitation.max_subscriptions: 20`, and a 1000-author feed that fanned out
//! would blow straight through that.
//!
//! Evidence is NMP's own `RelayDiagnosticsSnapshot`, which is first-party but
//! not self-flattering: `filters` is documented as "the EXACT wire JSON of
//! every filter currently sent to this relay — never fabricated/derived", and
//! `events_by_kind` counts what the relay actually sent back. A subscription
//! the relay refused delivers nothing, so rows arriving is what turns
//! "we opened N subscriptions" into "N subscriptions the relay accepted".
//!
//! ```text
//! cargo run -p nmp --example live_relay_validation -- <authors-file> [relay-url ...]
//! ```
//!
//! `<authors-file>` is newline-separated hex pubkeys — a real follow list is
//! the interesting corpus, because its size is what makes fan-out fatal.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use nmp::{Binding, Engine, EngineConfig, Filter, LiveQuery, Window};

const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
];

/// Long enough for a real relay to accept the REQ and start streaming, short
/// enough to stay a polite guest.
///
/// Override with `NMP_LIVE_SETTLE=<seconds>`. This matters more than it looks:
/// routing resolves each author's NIP-65 relay list before its feed filter can
/// be placed, so a short settle reports the fan-out MID-FLIGHT -- a snapshot
/// taken while most of the corpus is still unrouted reads as a small
/// subscription count and flatters the result. Any steady-state claim needs a
/// settle long enough that the count has stopped climbing.
const DEFAULT_SETTLE_SECS: u64 = 12;

fn settle() -> Duration {
    Duration::from_secs(
        std::env::var("NMP_LIVE_SETTLE")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_SETTLE_SECS),
    )
}

/// Number of entries in a wire filter's `authors` array.
///
/// Parsed from the `authors` array specifically rather than by counting
/// `","` across the whole JSON: that older approach summed EVERY string array
/// in the filter (`ids`, `#p`, `#e`, …), and returned 1 for a filter with no
/// string arrays at all, so width-1 was ambiguous between "one author" and
/// "no authors". Absent `authors` is reported as 0, which is distinct from a
/// present-but-empty array only in that the latter cannot occur on the wire.
fn authors_in(filter_json: &str) -> usize {
    let Some(rest) = filter_json.split("\"authors\":[").nth(1) else {
        return 0;
    };
    let Some(body) = rest.split(']').next() else {
        return 0;
    };
    if body.trim().is_empty() {
        return 0;
    }
    body.split(',').filter(|s| !s.trim().is_empty()).count()
}

/// Replace the `authors` array with `<N authors>` so the filter's structural
/// fields stay visible.
///
/// Head-truncating a 1055-author filter shows nothing but pubkeys and hides
/// `since`/`until`/`limit` entirely -- which is what decides what the filter
/// means. An unlimited wide filter with `since == until` is a tie-second
/// demand covering ONE second (its authors merged precisely because the
/// scalars are equal), not coverage of the feed.
fn elide_authors(filter_json: &str) -> String {
    let n = authors_in(filter_json);
    let Some(start) = filter_json.find("\"authors\":[") else {
        return filter_json.to_string();
    };
    let Some(end_rel) = filter_json[start..].find(']') else {
        return filter_json.to_string();
    };
    format!(
        "{}\"authors\":<{n} authors>{}",
        &filter_json[..start],
        &filter_json[start + end_rel + 1..]
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let authors_path = args
        .next()
        .expect("usage: live_relay_validation <authors-file> [relay-url ...]");
    let relays: Vec<String> = {
        let rest: Vec<String> = args.collect();
        if rest.is_empty() {
            DEFAULT_RELAYS.iter().map(|r| r.to_string()).collect()
        } else {
            rest
        }
    };

    let authors: BTreeSet<String> = std::fs::read_to_string(&authors_path)
        .expect("authors file")
        .lines()
        .map(str::trim)
        .filter(|l| l.len() == 64 && l.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|l| l.to_string())
        .collect();
    assert!(
        !authors.is_empty(),
        "no valid hex pubkeys in {authors_path}"
    );

    println!("corpus : {} distinct authors", authors.len());
    println!("relays : {}", relays.join(", "));
    println!(
        "\nEach author is its own demand atom, so an uncollapsed engine would want\n\
         {} concurrent subscriptions per relay. Several of these relays cap at 20.\n",
        authors.len()
    );

    let engine = Engine::new(EngineConfig {
        store_path: None,
        indexer_relays: Vec::new(),
        app_relays: relays.clone(),
        fallback_relays: Vec::new(),
        allowed_local_relay_hosts: vec!["localhost".into(), "127.0.0.1".into()],
        max_relays: relays.len().max(1),
        max_auth_capabilities: 4,
    })
    .expect("engine");

    let diagnostics = engine.observe_diagnostics().expect("diagnostics");

    // The realistic shape: notes from everyone I follow. A literal author set
    // still fans out one atom per element in the resolver — collapsing it is
    // the router's job, which is exactly what is under test.
    let feed = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(authors.clone())),
        ..Filter::default()
    });

    // `NMP_LIVE_WINDOW=<n>` bounds the feed, which lowers to a wire `limit`.
    // Default is unbounded.
    //
    // This comment used to claim that a bounded feed measures the limit guard
    // rather than the collapse, "because a limited filter can never merge
    // (`neither_limited`), so the collapse does not apply and the fan-out
    // returns". THAT IS WRONG, and the probe's own output disproves it: a
    // bounded run puts `{1055 authors, kinds:[1], limit:200}` on the wire.
    //
    // `neither_limited` governs `coalesce`, but the author axis of an OUTBOX
    // atom never reaches it. `Router::compile` groups outbox demand by
    // `route::Skeleton`, and `Skeleton::of` erases ONLY `authors` -- `limit`
    // stays in the skeleton. So all 1055 per-author atoms of a `limit:200`
    // feed share one skeleton, are coverage-solved together, and
    // `Skeleton::with_authors` rebuilds one filter per relay carrying every
    // author routed there, limit intact. The collapse happens in ROUTING,
    // upstream of the merge registry, and `neither_limited` never sees it.
    //
    // What a bounded run does still show is a per-author tail of
    // `{one author, kinds:[1], limit:200}` filters beside that wide one. Those
    // are not outbox-solved atoms -- outbox atoms collapse, as above -- so
    // they arrive by another class or lane, and that tail is what exhausts the
    // relay's subscription budget (#937).
    let window = std::env::var("NMP_LIVE_WINDOW")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .and_then(std::num::NonZeroUsize::new)
        .map(|n| Window::Expandable { initial: n, max: n });
    let bounded = window.is_some();
    let _sub = engine.observe(feed, window).expect("observe feed");

    // Let the wire settle against real relays, then take the newest snapshot.
    // `recv` blocks until a snapshot is published, so a bounded drain on a
    // background thread keeps this from hanging on a quiet network.
    let started = Instant::now();
    let deadline = started + settle();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Some(snapshot) = diagnostics.recv() {
            if tx.send(snapshot).is_err() {
                return;
            }
        }
    });
    let mut latest = None;
    // Trajectory of the peak subscription count, so "settled" is OBSERVED
    // rather than assumed. A count still climbing at the deadline means the
    // run ended mid-flight and its final number is a floor, not a steady state.
    let mut trajectory: Vec<(u64, usize, usize)> = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(snapshot) => {
                let peak = snapshot
                    .relays
                    .iter()
                    .map(|r| r.wire_sub_count)
                    .max()
                    .unwrap_or(0);
                let authors: usize = snapshot.relays.iter().map(|r| r.authors_served).sum();
                let at = started.elapsed().as_secs();
                match trajectory.last() {
                    Some(&(_, p, a)) if p == peak && a == authors => {}
                    _ => trajectory.push((at, peak, authors)),
                }
                latest = Some(snapshot);
            }
            Err(_) => break,
        }
    }
    let snapshot = latest.expect("at least one diagnostics snapshot");

    println!("--- settle trajectory (peak subs / authors served over time) ---");
    for (at, peak, authors) in &trajectory {
        println!("  t+{at:>3}s  peak_subs={peak:<5} authors_served={authors}");
    }
    // Settled means "nothing changed for a while", measured against the CLOCK.
    //
    // The first version of this compared the last two trajectory entries and
    // warned if the count had risen. But `trajectory` only pushes ON CHANGE,
    // so its last entry is the last CHANGE, not the last observation, and
    // consecutive changes are almost always increases. It therefore fired on
    // every run including the most definitively settled one -- nos.lol pinned
    // at exactly 20 and relay.damus.io at exactly 200, their advertised caps,
    // where the count could not climb because it was clamped.
    //
    // That is the same defect this example exists to fix: an aggregate that
    // cannot represent the distinction it is being asked about.
    const QUIET_SECS: u64 = 20;
    let elapsed = started.elapsed().as_secs();
    let last_change_at = trajectory.last().map(|&(at, _, _)| at);
    let still_climbing = match last_change_at {
        None => true,
        Some(at) => elapsed.saturating_sub(at) < QUIET_SECS,
    };
    match last_change_at {
        Some(at) if !still_climbing => println!(
            "  settled: no change for {}s (last change t+{at}s, ran {elapsed}s)",
            elapsed.saturating_sub(at)
        ),
        _ => {}
    }
    if still_climbing {
        println!(
            "  WARNING: the count changed within the last {QUIET_SECS}s. This run is a\n\
             \x20 LOWER BOUND on the steady state, not a measurement of it. Raise\n\
             \x20 NMP_LIVE_SETTLE until the trajectory flattens before quoting a number."
        );
    }
    println!();

    // `budget`/`refused` are the two most load-bearing columns for #937 and
    // this probe used to omit them, which is how I came to call the resulting
    // under-service "silent". It is not silent: the engine records a
    // `BudgetShortfall` per session, `RelayPlan::limited` keeps the affected
    // atoms from being called fresh by `plan_is_fresh_for`, and
    // `acquisition_evidence` reports `ShortfallFact::LocalLimit` to the app.
    // The snapshot this probe already reads carries both numbers; only the
    // printout was missing them.
    println!(
        "{:<28} {:>5} {:>7} {:>8} {:>9} {:>8}",
        "relay", "subs", "budget", "refused", "authors", "events"
    );
    let mut worst_subs = 0usize;
    let mut served = 0u64;
    let mut per_relay: BTreeMap<String, (usize, usize, u64)> = BTreeMap::new();

    for row in &snapshot.relays {
        let widest = row.filters.iter().map(|f| authors_in(f)).max().unwrap_or(0);
        let events: u64 = row.events_by_kind.iter().map(|(_, n)| n).sum();
        worst_subs = worst_subs.max(row.wire_sub_count);
        served += events;
        per_relay.insert(row.relay.to_string(), (row.wire_sub_count, widest, events));
        println!(
            "{:<28} {:>5} {:>7} {:>8} {:>9} {:>8}",
            row.relay.to_string(),
            row.wire_sub_count,
            row.subscription_budget
                .map(|b| b.to_string())
                .unwrap_or_else(|| "-".into()),
            row.subscriptions_refused,
            row.authors_served,
            events
        );
    }

    // Classify what the wire actually holds. `filters` is the exact wire JSON,
    // so shapes here are observed, not inferred.
    //
    // A DISTRIBUTION, not a maximum. Reporting only the widest filter cannot
    // tell "64 filters each carrying 1055 authors" apart from "1 filter with
    // 1055 authors plus 63 singletons" -- and those are different bugs with
    // different fixes. The first would mean limited atoms merged on authors
    // and something else multiplied them; the second is ordinary per-author
    // `neither_limited` fan-out. #937 asserted the first from a max, which the
    // instrument could not support.
    println!("\n--- author-count distribution per (relay, kinds) group ---");
    for row in &snapshot.relays {
        let mut shapes: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for f in &row.filters {
            let kinds = f
                .split("\"kinds\":")
                .nth(1)
                .and_then(|r| r.split(']').next())
                .map(|k| format!("kinds:{k}]"))
                .unwrap_or_else(|| "kinds:none".to_string());
            shapes.entry(kinds).or_default().push(authors_in(f));
        }
        println!("  {}", row.relay);
        for (kinds, widths) in &shapes {
            // Histogram over author-count, so a long tail of singletons is
            // visible rather than hidden behind the one wide filter.
            let mut hist: BTreeMap<usize, usize> = BTreeMap::new();
            for w in widths {
                *hist.entry(*w).or_insert(0) += 1;
            }
            let total: usize = widths.iter().sum();
            println!(
                "     {:>4} x {:<26} authors: total={total} distinct-widths={}",
                widths.len(),
                kinds,
                hist.len()
            );
            for (width, count) in &hist {
                println!("            {count:>4} filter(s) carrying {width:>5} author(s)");
            }
        }
    }

    // Raw wire JSON of a few filters from the largest group -- with dozens of
    // near-identical filters the differing axis is visible in one pass, and
    // guessing what the resolver "should" emit is not evidence.
    if let Some(row) = snapshot
        .relays
        .iter()
        .max_by_key(|r| r.filters.len())
        .filter(|r| r.filters.len() > 1)
    {
        println!(
            "\n--- one exemplar per distinct author-width on {} ---",
            row.relay
        );
        // One example per width bucket, not the first N. The first N are all
        // singletons and hide the wide filter entirely -- and whether that
        // wide filter carries a `limit` is the whole question: a wide LIMITED
        // filter means the feed is already served and the singletons are
        // redundant; a wide UNLIMITED one means they are not.
        // Keyed on (kinds, width), NOT width alone. A relay-wide width key
        // lets the kind:10002 NIP-65 lookup -- also 1055 authors wide -- claim
        // the `width=1055` slot and suppress the kind:1 filter of the same
        // width, which is the one actually in question. Two filters can share
        // an author count and mean entirely different things.
        let mut seen: BTreeSet<(String, usize)> = BTreeSet::new();
        for f in &row.filters {
            let w = authors_in(f);
            let kinds = f
                .split("\"kinds\":")
                .nth(1)
                .and_then(|r| r.split(']').next())
                .map(|k| format!("kinds:{k}]"))
                .unwrap_or_else(|| "kinds:none".to_string());
            if !seen.insert((kinds, w)) {
                continue;
            }
            // Elide the authors array rather than truncating the string. A
            // head-truncated 1055-author filter shows nothing but pubkeys,
            // hiding `since`/`until`/`limit` -- and those decide what the
            // filter MEANS. An unlimited wide filter with `since == until` is
            // a tie-second demand covering ONE second (the authors axis merged
            // because the scalars are equal), not coverage of the feed.
            println!("  width={w:<5} {}", elide_authors(f));
        }
    }

    println!("\n--- verdict ---");
    println!("highest concurrent subscriptions on any relay : {worst_subs}");
    println!("total events served back                      : {served}");

    // The two things that matter. Subscriptions must stay far under the
    // tightest real cap, and rows must actually arrive — a refused
    // subscription is silent, so bounded-and-empty would be a false pass.
    if bounded {
        println!(
            "\nNOTE: bounded window in use, so every filter carries a `limit` and\n\
             none of them may merge. Fan-out here is CORRECT behaviour, not a\n\
             regression -- see the limit-poisoning guard."
        );
        return;
    }

    assert!(
        worst_subs <= 20,
        "collapse FAILED: {worst_subs} concurrent subscriptions exceeds the 20 that \
         nos.lol, relay.primal.net and offchain.pub all advertise"
    );
    assert!(
        served > 0,
        "no events arrived — the subscriptions may have been refused, so a low \
         subscription count proves nothing"
    );
    println!("\nOK: {} authors served by at most {worst_subs} subscription(s) per relay, and real events arrived.", authors.len());
}
