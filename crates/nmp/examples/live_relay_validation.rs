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
const SETTLE: Duration = Duration::from_secs(12);

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

    // NO window on purpose. A bounded window lowers to a wire `limit`, and a
    // limited filter can never merge with anything (`neither_limited`) --
    // because two `limit:200` REQs promise 200 rows each while their union
    // promises 200 total, so merging would silently under-fetch. Asking for a
    // bounded feed here would therefore measure the limit guard rather than
    // the collapse, and would report fan-out that is correct behaviour.
    // `NMP_LIVE_WINDOW=<n>` bounds the feed, which lowers to a wire `limit`.
    // That is the interesting negative case: a limited filter can never merge
    // (`neither_limited`), so the collapse does not apply and the fan-out
    // returns. Default is unbounded.
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
    let deadline = Instant::now() + SETTLE;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Some(snapshot) = diagnostics.recv() {
            if tx.send(snapshot).is_err() {
                return;
            }
        }
    });
    let mut latest = None;
    while Instant::now() < deadline {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(snapshot) => latest = Some(snapshot),
            Err(_) => break,
        }
    }
    let snapshot = latest.expect("at least one diagnostics snapshot");

    println!(
        "{:<28} {:>5} {:>7} {:>9} {:>8}",
        "relay", "subs", "widest", "authors", "events"
    );
    let mut worst_subs = 0usize;
    let mut served = 0u64;
    let mut per_relay: BTreeMap<String, (usize, usize, u64)> = BTreeMap::new();

    for row in &snapshot.relays {
        let widest = row
            .filters
            .iter()
            .map(|f| f.matches("\",\"").count() + 1)
            .max()
            .unwrap_or(0);
        let events: u64 = row.events_by_kind.iter().map(|(_, n)| n).sum();
        worst_subs = worst_subs.max(row.wire_sub_count);
        served += events;
        per_relay.insert(row.relay.to_string(), (row.wire_sub_count, widest, events));
        println!(
            "{:<28} {:>5} {:>7} {:>9} {:>8}",
            row.relay.to_string(),
            row.wire_sub_count,
            widest,
            row.authors_served,
            events
        );
    }

    // Classify what the wire actually holds. `filters` is the exact wire JSON,
    // so shapes here are observed, not inferred.
    println!("\n--- filter shapes per relay (kinds -> count, widest value array) ---");
    for row in &snapshot.relays {
        let mut shapes: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for f in &row.filters {
            let kinds = f
                .split("\"kinds\":")
                .nth(1)
                .and_then(|r| r.split(']').next())
                .map(|k| format!("kinds:{k}]"))
                .unwrap_or_else(|| "kinds:none".to_string());
            let width = f.matches("\",\"").count() + 1;
            let e = shapes.entry(kinds).or_insert((0, 0));
            e.0 += 1;
            e.1 = e.1.max(width);
        }
        println!("  {}", row.relay);
        for (k, (n, w)) in &shapes {
            println!("     {n:>4} x {k:<32} widest={w}");
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
