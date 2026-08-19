//! Shared scenario vocabulary. `mod support;` from each test file.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use nmp::{
    Binding, Demand, Engine, EngineConfig, Filter, LiveQuery, ReadRouting, Row, RowDelta,
    Subscription,
};
use nmp_relay_lab::RelayLab;
use nostr::{Event, EventBuilder, Keys, RelayUrl, Timestamp};

/// How long a scenario waits for the engine to do something it should do
/// promptly. Generous, because the assertion is never "it was fast".
pub const SETTLE: Duration = Duration::from_secs(20);
/// How long the wire must stay silent before a count off it is settled.
pub const QUIET: Duration = Duration::from_millis(250);

/// A signed kind:1 note at a stated instant.
pub fn note(author: &Keys, content: &str, created_at: u64) -> Event {
    EventBuilder::text_note(content)
        .custom_created_at(Timestamp::from_secs(created_at))
        .sign_with_keys(author)
        .expect("a fixture note signs cleanly")
}

/// `n` notes by one author, one second apart, oldest first.
pub fn notes(author: &Keys, n: usize, from: u64) -> Vec<Event> {
    (0..n)
        .map(|i| note(author, &format!("note {i}"), from + i as u64))
        .collect()
}

/// An engine whose only relay is this one.
pub fn engine_against(relay: &RelayLab) -> Engine {
    Engine::new(EngineConfig {
        app_relays: vec![relay.url().to_string()],
        ..EngineConfig::default()
    })
    .expect("an engine with one app relay builds")
}

/// A literal `kinds:[1], authors:[author]` live query, PINNED to one relay.
///
/// Explicit rather than `ReadRouting::Auto`, and that is not incidental: an
/// engine built from an `EngineConfig` naming this relay in `app_relays`
/// sends it writes but issues no READ against it for a query like this one --
/// the socket is never even opened. Every read scenario here therefore pins
/// its relay, the same way `crates/nmp/tests/finished_stored_events.rs` and
/// `integration_capstone.rs` do. See this crate's report: it is a finding
/// about the routing surface, not a property of the harness.
pub fn kind1_by_on(author: &Keys, relay: &RelayUrl) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([1u16])),
                authors: Some(Binding::Literal(BTreeSet::from([author
                    .public_key()
                    .to_hex()]))),
                ..Filter::default()
            },
            ReadRouting::Explicit(vec![relay.clone()]),
        )
        .expect("a one-relay pinned set is nonempty"),
    )
}

/// A literal one-kind, one-author query pinned to `relay`.
pub fn kind_by_on(author: &Keys, relay: &RelayLab, kind: u16) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([kind])),
                authors: Some(Binding::Literal(BTreeSet::from([author
                    .public_key()
                    .to_hex()]))),
                ..Filter::default()
            },
            ReadRouting::Explicit(vec![relay.url().clone()]),
        )
        .expect("a one-relay pinned set is nonempty"),
    )
}

/// [`kind1_by_on`] against the relay the scenario is driving.
pub fn kind1_by(author: &Keys, relay: &RelayLab) -> LiveQuery {
    kind1_by_on(author, relay.url())
}

/// Drain the subscription until `budget` elapses, returning every row the app
/// was ever shown, newest state per event id, in first-seen order.
///
/// Deliberately drains the WHOLE budget rather than stopping at a count: a
/// scenario about silent truncation is asserting the app was shown no more,
/// and stopping as soon as the expected number arrived would make that
/// assertion unfalsifiable.
pub fn rows_within(subscription: &Subscription, budget: Duration) -> Vec<Row> {
    let deadline = Instant::now() + budget;
    let mut rows: Vec<Row> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return rows;
        }
        let Ok(frame) = subscription.recv_timeout(remaining) else {
            return rows;
        };
        for delta in frame.deltas {
            match delta {
                RowDelta::Added(row) => rows.push(row),
                RowDelta::Updated(row) => {
                    if let Some(slot) = rows.iter_mut().find(|held| held.id() == row.id()) {
                        *slot = row;
                    }
                }
                _ => {}
            }
        }
    }
}

/// Drain until at least `want` distinct rows have arrived, or `budget`
/// elapses. For the scenarios where the count is a floor, not a ceiling.
pub fn rows_until(subscription: &Subscription, want: usize, budget: Duration) -> Vec<Row> {
    let deadline = Instant::now() + budget;
    let mut rows: Vec<Row> = Vec::new();
    while rows.len() < want {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return rows;
        }
        let Ok(frame) = subscription.recv_timeout(remaining) else {
            return rows;
        };
        for delta in frame.deltas {
            if let RowDelta::Added(row) = delta {
                rows.push(row);
            }
        }
    }
    rows
}

/// An engine with a local account, ready to publish.
pub fn publishing_engine(relay: &RelayLab, keys: &Keys) -> Engine {
    let engine = engine_against(relay);
    engine
        .add_private_key_account(&keys.secret_key().to_secret_bytes(), true)
        .expect("the account and its local provider register");
    engine
}

/// A kind:1 write intent whose destination NMP resolves itself.
pub fn publish_note(engine: &Engine, content: &str) -> nmp::ReceiptStream {
    engine
        .publish(nmp::WriteIntent {
            payload: nmp::WritePayload::Event(nmp::EventBuilder {
                kind: nostr::Kind::TextNote,
                tags: Vec::new(),
                content: content.to_string(),
                created_at: None,
            }),
            routing: nmp::WriteRouting::Auto,
            identity: nmp::Identity::Active,
        })
        .expect("the write is accepted")
}

/// Drain a receipt until one relay fact for this relay is terminal, or the
/// budget runs out. Returns every fact seen, so a failure reports the whole
/// history rather than "timed out".
pub fn relay_facts(
    receipts: &nmp::FifoReceiver<nmp::WriteFact>,
    budget: Duration,
) -> Vec<nmp::RelayState> {
    let deadline = Instant::now() + budget;
    let mut states = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return states;
        }
        match receipts.recv_timeout(remaining) {
            Ok(nmp::WriteFact::Relay { state, .. }) => {
                let terminal = matches!(
                    state,
                    nmp::RelayState::Published
                        | nmp::RelayState::Rejected { .. }
                        | nmp::RelayState::GaveUp
                );
                states.push(state);
                if terminal {
                    return states;
                }
            }
            Ok(_) => {}
            Err(_) => return states,
        }
    }
}
