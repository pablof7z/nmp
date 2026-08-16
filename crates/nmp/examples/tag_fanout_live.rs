//! Live probe: what NMP actually puts on a relay socket when many
//! subscriptions differ only in one tag value, versus only in `authors`.
//!
//! Run a logging relay, then this:
//!
//! ```text
//! nak serve --port 10547 --verbose
//! cargo run -p nmp --example tag_fanout_live -- ws://localhost:10547
//! ```
//!
//! The relay's own log is the evidence — not this program's output, and not
//! anything read out of the router.
//!
//! What to expect: ONE REQ per mode, widened in place as each subscription
//! opens, in BOTH modes. This probe was written when the tag mode produced
//! one REQ per value (20 demands -> 23 REQs, one value each) against an
//! author mode that accumulated `[A]` -> `[A,B]` -> `[A,B,C]` on a single
//! subscription id; `nmp_router::StructuralUnion` made the two modes the same
//! measurement. If they diverge again, the tag axis has lost its merge.
//!
//! Optional args:
//!
//! ```text
//! tag_fanout_live <relay-url> [count] [gap-millis] [mode]
//!   mode: tag (default) | authors | both
//! ```
//!
//! `gap-millis` is the delay between opening consecutive subscriptions,
//! which is what reveals whether aggregation has a time window at all.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use nmp::{
    AccessContext, Binding, Demand, Engine, EngineConfig, Filter, IndexedTagName, LiveQuery,
    RelayUrl, SourceAuthority, Subscription,
};

fn hex32(i: usize) -> String {
    format!("{i:064x}")
}

/// `kinds:[1], #p:[hex32(i)]`, pinned to `relay`.
fn tag_query(relay: &RelayUrl, i: usize) -> LiveQuery {
    let filter = Filter {
        kinds: Some(BTreeSet::from([1u16])),
        tags: BTreeMap::from([(
            IndexedTagName::new('p').unwrap(),
            Binding::Literal(BTreeSet::from([hex32(i)])),
        )]),
        ..Filter::default()
    };
    pinned(relay, filter)
}

/// `kinds:[1], authors:[hex32(i)]`, pinned to `relay` — the control.
fn author_query(relay: &RelayUrl, i: usize) -> LiveQuery {
    let filter = Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([hex32(i)]))),
        ..Filter::default()
    };
    pinned(relay, filter)
}

fn pinned(relay: &RelayUrl, filter: Filter) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            filter,
            SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
            AccessContext::Public,
        )
        .expect("pinned demand with a nonempty relay set"),
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let relay_arg = args.next().unwrap_or_else(|| "ws://localhost:10547".into());
    let count: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(8);
    let gap_ms: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(0);
    let mode = args.next().unwrap_or_else(|| "tag".into());

    let relay = RelayUrl::parse(&relay_arg).expect("relay url");

    let engine = Engine::new(EngineConfig {
        store_path: None,
        app_relays: vec![relay_arg.clone()],
        fallback_relays: Vec::new(),
        // A loopback dev relay must be opted into explicitly.
        max_relays: 4,
        max_auth_capabilities: 4,
        max_publish_attempts: nmp::DEFAULT_MAX_PUBLISH_ATTEMPTS,
    })
    .expect("engine");

    println!("relay      : {relay_arg}");
    println!("mode       : {mode}");
    println!("count      : {count}");
    println!("gap        : {gap_ms}ms between opens");
    println!("--- opening subscriptions; watch the relay log ---");

    // Held for the process lifetime: dropping a Subscription closes it.
    let mut held: Vec<Subscription> = Vec::new();

    let open_tags = mode == "tag" || mode == "both";
    let open_authors = mode == "authors" || mode == "both";

    if open_tags {
        for i in 0..count {
            held.push(
                engine
                    .observe(tag_query(&relay, i), None)
                    .expect("observe tag query"),
            );
            if gap_ms > 0 {
                std::thread::sleep(Duration::from_millis(gap_ms));
            }
        }
        println!("opened {count} subscriptions differing only in #p");
    }

    if open_authors {
        for i in 0..count {
            held.push(
                engine
                    .observe(author_query(&relay, i), None)
                    .expect("observe author query"),
            );
            if gap_ms > 0 {
                std::thread::sleep(Duration::from_millis(gap_ms));
            }
        }
        println!("opened {count} subscriptions differing only in authors");
    }

    // Let every REQ reach the socket before the process (and its
    // subscriptions) go away.
    std::thread::sleep(Duration::from_secs(3));
    println!("--- done; {} subscription handles held ---", held.len());
}
