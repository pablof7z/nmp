//! #1137's wire capstone for retired-lane suppression: a replaceable write
//! superseded before its own first delivery attempt must never reach a
//! relay, proven by an INDEPENDENT socket witness rather than receipt facts
//! alone.
//!
//! #1137's own audit found every existing scenario for this claim stopped at
//! the receipt stream: "the four-row outbox outline proves
//! `Superseded`/waiting/`Acked` receipt facts. It never independently
//! inspects the relay's EVENT set, so a reducer that emits `Superseded` yet
//! later sends the retired older event could pass." This file closes that
//! gap: it asserts on [`nmp_test_support::relays::ScriptedRelay::wire_record`]
//! and `admitted_events` -- the relay's own account of which bytes actually
//! arrived -- not on what the reducer says happened.
//!
//! Sequence: with nothing listening on the target port, accept an OLDER
//! plain `WritePayload::Event` at a fresh replaceable coordinate, then
//! accept a NEWER one at the same coordinate -- both BEFORE the relay
//! exists, so neither could possibly have started an attempt yet
//! (`retire_superseded_owners_in_txn`'s `handoff_may_have_occurred` check is
//! trivially false for a port nothing has ever listened on). This is the
//! ordinary address-index winner/retirement mechanism every replaceable or
//! addressable write goes through -- it owed nothing to the CAS-guarded
//! `WritePayload::ReplaceableEdit` mechanism #1137's own investigation found
//! this test's ORIGINAL version had merely borrowed for convenience, and
//! which is now deleted; the retirement machinery under test here is
//! unaffected by that deletion. Only then does the relay come up on that
//! exact port. The engine's retry loop reaches it and the independent
//! socket witness sees exactly one `EVENT` -- the newer's -- ever, at any
//! point, including rejected/pre-admission frames.
//!
//! nmp:falsifier=A replaceable-coordinate write retired before its own first
//! delivery attempt never reaches the wire, proven by the relay's own
//! independent record of every EVENT frame it has seen -- not by receipt
//! facts alone.

use std::net::TcpListener;
use std::time::{Duration, Instant};

use nmp::mechanism::runtime::FifoReceiver;
use nmp::{
    Engine, EngineConfig, Identity, NotSentReason, WriteFact, WriteIntent, WriteOutcome,
    WritePayload, WriteRouting,
};
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::{Keys, Kind, RelayUrl, Timestamp};

const RETRY_SETTLE: Duration = Duration::from_secs(90);

fn engine() -> Engine {
    Engine::new(EngineConfig::default()).expect("a temporary Redb engine builds")
}

/// A loopback URL with nothing listening on it yet -- the exact
/// `two_unreachable_hosts` trick `optimistic_publish.rs` uses, sized to one
/// host: bind to grab a free port, record the URL, then drop the listener so
/// the OS keeps the port free for `ScriptedRelay::start_on_port` to bind to
/// later.
fn one_unreachable_host() -> RelayUrl {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback free port");
    let port = listener
        .local_addr()
        .expect("bound listener has an address")
        .port();
    drop(listener);
    RelayUrl::parse(&format!("ws://127.0.0.1:{port}")).expect("well-formed loopback relay url")
}

fn port_of(url: &RelayUrl) -> u16 {
    url.as_str_without_trailing_slash()
        .rsplit(':')
        .next()
        .and_then(|port| port.parse().ok())
        .expect("a loopback relay url carries an explicit port")
}

fn drain_until(
    receipts: &FifoReceiver<WriteFact>,
    timeout: Duration,
    mut pred: impl FnMut(&WriteFact) -> bool,
) -> Vec<WriteFact> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("the receipt stream never satisfied the predicate; saw {seen:?}");
        }
        match receipts.recv_timeout(remaining) {
            Ok(status) => {
                let done = pred(&status);
                seen.push(status);
                if done {
                    return seen;
                }
            }
            Err(error) => panic!("the receipt stream ended early ({error:?}); saw {seen:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lane_retired_before_its_first_attempt_never_reaches_the_wire() {
    let author = Keys::generate();
    let relay_url = one_unreachable_host();

    let engine = engine();
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("register the provider and select its account");

    // Older: the first value at a fresh replaceable coordinate. Nothing is
    // listening on `relay_url` yet, so this can never have started a
    // delivery attempt.
    let older = engine
        .publish(WriteIntent {
            payload: WritePayload::Event(
                nmp_grammar::EventBuilder::new(Kind::ContactList)
                    .content("older -- must never reach the wire")
                    .created_at(Timestamp::from(1_700_000_000)),
            ),
            routing: WriteRouting::Explicit(vec![relay_url.clone()]),
            identity: Identity::Explicit(author.public_key()),
            correlation: None,
        })
        .expect("the first write at a fresh coordinate is accepted");
    let older_id = older.event_id;

    // Newer: the same replaceable coordinate, still before the relay
    // exists. No precondition is stated or needed -- the ordinary
    // address-index winner/retirement mechanism supersedes the older write
    // regardless of which payload variant accepted it.
    let newer = engine
        .publish(WriteIntent {
            payload: WritePayload::Event(
                nmp_grammar::EventBuilder::new(Kind::ContactList)
                    .content("newer -- the only one that may reach the wire")
                    .created_at(Timestamp::from(1_700_000_100)),
            ),
            routing: WriteRouting::Explicit(vec![relay_url.clone()]),
            identity: Identity::Explicit(author.public_key()),
            correlation: None,
        })
        .expect("the second write at the same coordinate is accepted");
    let newer_id = newer.event_id;
    assert_ne!(older_id, newer_id, "fixture sanity: two distinct events");

    // The older write's own receipt stream must report NotSent(Superseded)
    // -- a local, store-only decision that needs no relay at all, and never
    // even attempts a delivery lane.
    let older_statuses = drain_until(&older.statuses, RETRY_SETTLE, |fact| {
        matches!(
            fact,
            WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Superseded))
        )
    });
    assert!(
        older_statuses
            .iter()
            .all(|fact| !matches!(fact, WriteFact::Relay { .. })),
        "the retired write must report NotSent(Superseded) without EVER \
         reporting a per-relay fact first -- got {older_statuses:?}"
    );

    // Only now does the relay come up, on the exact port both writes were
    // aimed at, so the engine's retry loop reaches it.
    let relay = ScriptedRelay::start_on_port(port_of(&relay_url), &RelayConfig::default()).await;
    assert_eq!(
        relay.url, relay_url,
        "the relay came up where the writes aimed"
    );

    // Independent socket witness: wait for the newer event's bytes to
    // actually arrive on the wire.
    let saw_newer = relay
        .wait_wire_event_id_count(&newer_id.to_hex(), 1, RETRY_SETTLE)
        .await;
    assert!(
        saw_newer,
        "the newer event never reached the wire within {RETRY_SETTLE:?}"
    );

    // The newer write's own receipt stream must confirm Published too --
    // ordinary confirmation alongside the independent wire witness above,
    // not a replacement for it.
    drain_until(&newer.statuses, RETRY_SETTLE, |fact| {
        matches!(
            fact,
            WriteFact::Relay {
                relay: fact_relay,
                state: nmp::RelayState::Published,
                ..
            } if fact_relay == &relay_url
        )
    });

    // The actual falsifier: the relay's own independent record, not the
    // reducer's receipts. Every raw wire EVENT frame this relay has EVER
    // seen -- including anything it might have rejected before admission --
    // must be the newer id, and the newer id alone.
    let wire = relay.wire_record();
    let older_hex = older_id.to_hex();
    let newer_hex = newer_id.to_hex();
    assert!(
        !wire.event_ids.contains(&older_hex),
        "#1137: the retired older event reached the wire -- raw EVENT frames \
         seen: {:?}",
        wire.event_ids
    );
    assert!(
        wire.event_ids.iter().all(|id| id == &newer_hex),
        "#1137: an EVENT frame other than the newer id reached the wire -- \
         raw EVENT frames seen: {:?}",
        wire.event_ids
    );

    // And the relay's own write-policy admission log agrees: exactly one
    // event, the newer one.
    let admitted = relay.admitted_events();
    assert_eq!(
        admitted.len(),
        1,
        "#1137: the relay admitted more or fewer than exactly one EVENT -- {admitted:?}"
    );
    assert_eq!(
        admitted[0].id, newer_id,
        "#1137: the relay admitted an event other than the newer one"
    );

    engine.shutdown();
    relay.shutdown();
}
