//! #1122's PROTOCOL-GROUPISANIDENTITY-001/002/004 and
//! PROTOCOL-NIP29OPERATIONS-009/010: the app-facing group door
//! (`crates/nmp/src/nip29/{mod,group}.rs`, #1033) is an IDENTITY value, not
//! a subscription -- constructing one contacts nothing, writing through it
//! never needs a prior read, one retained handle serves the room's whole
//! lifetime with no lifecycle of its own, and a relay's own moderation
//! refusal reaches the app as an exact per-relay fact rather than a routing
//! failure or a silent acceptance.
//!
//! Companion to `group_publication_door.rs` (#1033's own wire-shape/routing
//! falsifiers) and `group_write_survives_a_refusing_read.rs`
//! (PROTOCOL-GROUPISANIDENTITY-003). Real in-process relays throughout, same
//! `nmp-test-support::relays::ScriptedRelay` harness, same
//! never-`nostr_relay_builder::prelude::*` precaution (`nmp-test-support`
//! owns the cross-version bridge).

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use nmp::mechanism::runtime::FifoReceiver;
use nmp::nip29;
use nmp::{
    Engine, EngineConfig, EventBuilder, Filter, RelayState, RelayWaiting, SigningState, WriteFact,
};
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::{Keys, Kind, PublicKey, RelayUrl};

const GROUP_ID: &str = "photographers";

/// Long enough for a real connect/publish/ack round trip on a loaded CI
/// runner, short enough that a genuine failure reports rather than hangs.
const SETTLE: Duration = Duration::from_secs(20);

fn bare_engine() -> Engine {
    Engine::new(EngineConfig {
        ..EngineConfig::default()
    })
    .expect("an in-memory engine builds")
}

/// A bare engine, plus one registered signing capability for `keys` -- what
/// `Identity::Explicit(keys.public_key())` (every `Group::publish` call,
/// every named operation included) needs to actually produce a signature.
fn engine_with_signer_for(keys: &Keys) -> Engine {
    let engine = bare_engine();
    engine
        .add_private_key_account(&keys.secret_key().to_secret_bytes(), false)
        .expect("the account and local provider register");
    engine
}

fn author() -> PublicKey {
    Keys::generate().public_key()
}

fn drain_until(
    receipts: &FifoReceiver<WriteFact>,
    pred: impl Fn(&WriteFact) -> bool,
) -> Vec<WriteFact> {
    let deadline = Instant::now() + SETTLE;
    let mut seen = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("receipt stream never satisfied the predicate; saw {seen:?}");
        }
        match receipts.recv_timeout(remaining) {
            Ok(status) => {
                let done = pred(&status);
                seen.push(status);
                if done {
                    return seen;
                }
            }
            Err(error) => panic!("receipt stream ended early ({error:?}); saw {seen:?}"),
        }
    }
}

fn relays_named_by(statuses: &[WriteFact]) -> BTreeSet<RelayUrl> {
    let mut named = BTreeSet::new();
    for status in statuses {
        match status {
            WriteFact::Destinations { relays, .. } => named.extend(relays.iter().cloned()),
            WriteFact::Relay { relay, .. } => {
                named.insert(relay.clone());
            }
            WriteFact::Signing(_) | WriteFact::Outcome(_) => {}
        }
    }
    named
}

fn wait_for_events(relay: &ScriptedRelay, count: usize) -> Vec<nostr::Event> {
    let deadline = Instant::now() + SETTLE;
    loop {
        let admitted = relay.admitted_events();
        if admitted.len() >= count {
            return admitted;
        }
        assert!(
            Instant::now() < deadline,
            "relay {} admitted {} of {count} expected events",
            relay.url,
            admitted.len()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn h_rows(event: &nostr::Event) -> Vec<String> {
    event
        .tags
        .iter()
        .map(|tag| tag.clone().to_vec())
        .filter(|row| row.first().map(String::as_str) == Some("h"))
        .map(|row| row.get(1).cloned().unwrap_or_default())
        .collect()
}

/// PROTOCOL-GROUPISANIDENTITY-001. Naming a relay scope and narrowing it to
/// a group id is pure value construction: neither `nip29::on` nor
/// `RelayScope::group` takes an `Engine`, so there is no spelling that could
/// reach the network. Proved against a REAL running relay rather than taken
/// on the type signature alone, so a hidden global/lazy connection would
/// still be caught.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn constructing_a_group_scope_and_a_group_contacts_no_relay() {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;

    let scope = nip29::on([relay.url.clone()]).expect("one host forms a scope");
    let _group = scope.group(GROUP_ID);

    assert!(
        !relay.contacted(),
        "constructing a scope and a group must not contact the relay at all"
    );
    assert_eq!(relay.contact_count(), 0);
    assert!(
        relay.wire_record().reqs.is_empty(),
        "no query or subscription was ever sent"
    );

    relay.shutdown();
}

/// PROTOCOL-GROUPISANIDENTITY-002. A join request reaches its host with NO
/// prior (or concurrent) subscription -- `engine.observe` is never called at
/// all in this test. Proved on the wire: the relay's own decoded REQ log
/// must stay empty even after the write is fully acked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_request_is_publishable_with_no_subscription_at_all() {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let writer_keys = Keys::generate();
    let writer = writer_keys.public_key();
    let engine = engine_with_signer_for(&writer_keys);

    let group = nip29::on([relay.url.clone()])
        .expect("one host forms a scope")
        .group(GROUP_ID);
    let receipts = group
        .join_request(&engine, writer, Some("dark-slide-42"))
        .expect("a join request is accepted with no prior read")
        .statuses;
    let statuses = drain_until(&receipts, |status| {
        matches!(
            status,
            WriteFact::Relay {
                relay: _,
                state: RelayState::Published
            }
        )
    });
    assert_eq!(
        relays_named_by(&statuses),
        BTreeSet::from([relay.url.clone()])
    );

    let delivered = wait_for_events(&relay, 1);
    assert_eq!(delivered.len(), 1, "exactly the one join request");
    assert_eq!(delivered[0].kind.as_u16(), 9021);
    assert_eq!(delivered[0].pubkey, writer);

    assert!(
        relay.wire_record().reqs.is_empty(),
        "no subscription existed at any point during the publication"
    );

    engine.shutdown();
    relay.shutdown();
}

/// PROTOCOL-GROUPISANIDENTITY-004. ONE retained `Group` value -- never
/// reconstructed -- mints two independent reads and two independent writes
/// across the test, and keeps minting writes after both of its earlier
/// reads' subscriptions have already been cancelled: the handle owns no
/// lifecycle tied to any subscription it once produced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_retained_group_handle_mints_every_read_and_write_with_no_lifecycle_of_its_own() {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let writer_keys = Keys::generate();
    let writer = writer_keys.public_key();
    let engine = engine_with_signer_for(&writer_keys);

    // ONE handle. Every statement below reuses this same binding; none
    // reconstructs it.
    let group = nip29::on([relay.url.clone()])
        .expect("one host forms a scope")
        .group(GROUP_ID);

    // Read 1.
    let query1 = group
        .read(Filter {
            kinds: Some(BTreeSet::from([9u16])),
            ..Filter::default()
        })
        .expect("first read, from the retained handle");
    let sub1 = engine
        .observe(query1, None)
        .expect("first observe succeeds");

    // Write 1, same handle.
    let receipts1 = group
        .publish(
            &engine,
            writer,
            EventBuilder::new(Kind::from(9u16)).content("first"),
        )
        .expect("first write, from the retained handle")
        .statuses;
    drain_until(&receipts1, |s| {
        matches!(
            s,
            WriteFact::Relay {
                relay: _,
                state: RelayState::Published
            }
        )
    });

    // Read 2, a DIFFERENT filter, same handle -- no reconstruction.
    let query2 = group
        .read(Filter {
            kinds: Some(BTreeSet::from([10u16])),
            ..Filter::default()
        })
        .expect("second read, from the SAME retained handle");
    let sub2 = engine
        .observe(query2, None)
        .expect("second observe succeeds");

    // Both earlier subscriptions are withdrawn now. If the group handle
    // secretly owned any lifecycle tied to them, the next write would be
    // the place that would show it.
    drop(sub1);
    drop(sub2);

    // Write 2, the SAME handle, AFTER both reads' subscriptions are gone.
    let receipts2 = group
        .publish(
            &engine,
            writer,
            EventBuilder::new(Kind::from(10u16)).content("second"),
        )
        .expect("second write, from the retained handle, after both reads were dropped")
        .statuses;
    drain_until(&receipts2, |s| {
        matches!(
            s,
            WriteFact::Relay {
                relay: _,
                state: RelayState::Published
            }
        )
    });

    let delivered = wait_for_events(&relay, 2);
    assert_eq!(
        delivered.len(),
        2,
        "both writes, from the one retained handle, reached the host"
    );
    for event in &delivered {
        assert_eq!(
            h_rows(event),
            vec![GROUP_ID.to_string()],
            "both writes carry the SAME group id -- minted by the one retained handle"
        );
    }
    let contents: BTreeSet<&str> = delivered.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(contents, BTreeSet::from(["first", "second"]));

    engine.shutdown();
    relay.shutdown();
}

/// PROTOCOL-NIP29OPERATIONS-009. The host refuses a moderation action
/// (kind:9001, `remove_users`) with its own restriction message. The receipt
/// reports the rejection verbatim from that exact host, and the write is
/// never also reported accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_moderation_rejection_reports_the_hosts_exact_message_and_is_never_accepted() {
    let message = "restricted: not an admin of this group";
    let relay = ScriptedRelay::start(&RelayConfig {
        reject_kind: Some((9001, message.to_string())),
        ..RelayConfig::default()
    })
    .await;
    let moderator_keys = Keys::generate();
    let moderator = moderator_keys.public_key();
    let subject = author();
    let engine = engine_with_signer_for(&moderator_keys);

    let group = nip29::on([relay.url.clone()])
        .expect("one host forms a scope")
        .group(GROUP_ID);
    let receipts = group
        .remove_users(&engine, moderator, [subject])
        .expect("the door accepts the intent even though the host will refuse it")
        .statuses;

    let statuses = drain_until(&receipts, |status| {
        matches!(
            status,
            WriteFact::Relay {
                state: RelayState::Rejected { .. },
                ..
            }
        )
    });
    let rejection = statuses
        .iter()
        .find_map(|status| match status {
            WriteFact::Relay {
                relay: host,
                state: RelayState::Rejected { reason: msg },
            } if *host == relay.url => Some(msg.clone()),
            _ => None,
        })
        .expect("a Rejected status naming the host must be present");
    assert_eq!(
        rejection, message,
        "the receipt carries the host's own rejection message verbatim"
    );
    assert!(
        !statuses.iter().any(|s| matches!(
            s,
            WriteFact::Relay {
                relay: _,
                state: RelayState::Published
            }
        )),
        "the removal must never also be reported accepted: saw {statuses:?}"
    );

    engine.shutdown();
    relay.shutdown();
}

/// PROTOCOL-NIP29OPERATIONS-010. The SAME refusal, checked from the other
/// angle: it is reported as a per-relay REJECTION (not a routing failure),
/// and a second host in the SAME scope that accepts the write is completely
/// independent of the first host's refusal -- one relay's own moderation
/// decision, never rolled up into a claim about anything else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_moderation_rejection_is_a_host_fact_not_a_routing_failure() {
    let message = "restricted: not an admin of this group";
    let refusing = ScriptedRelay::start(&RelayConfig {
        reject_kind: Some((9001, message.to_string())),
        ..RelayConfig::default()
    })
    .await;
    let accepting = ScriptedRelay::start(&RelayConfig::default()).await;
    let moderator_keys = Keys::generate();
    let moderator = moderator_keys.public_key();
    let subject = author();
    let engine = engine_with_signer_for(&moderator_keys);

    let group = nip29::on([refusing.url.clone(), accepting.url.clone()])
        .expect("two hosts form a scope")
        .group(GROUP_ID);
    let receipts = group
        .remove_users(&engine, moderator, [subject])
        .expect("the door accepts the intent for the whole scope")
        .statuses;

    let expected = BTreeSet::from([refusing.url.clone(), accepting.url.clone()]);
    let mut seen = Vec::new();
    let mut rejected = false;
    let mut accepted = false;
    let deadline = Instant::now() + SETTLE;
    loop {
        if rejected && accepted {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "receipt stream never reported both hosts' independent facts; saw {seen:?}"
        );
        match receipts.recv_timeout(remaining) {
            Ok(status) => {
                match &status {
                    WriteFact::Relay {
                        relay,
                        state: RelayState::Rejected { reason: msg },
                    } if *relay == refusing.url => {
                        assert_eq!(msg, message);
                        rejected = true;
                    }
                    WriteFact::Relay {
                        relay,
                        state: RelayState::Published,
                    } if *relay == accepting.url => accepted = true,
                    _ => {}
                }
                seen.push(status);
            }
            Err(error) => panic!("receipt stream ended early ({error:?}); saw {seen:?}"),
        }
    }

    assert!(
        !seen.iter().any(|s| matches!(
            s,
            WriteFact::Relay {
                state: RelayState::GaveUp
                    | RelayState::Waiting(RelayWaiting::PersistenceStalled { .. }),
                ..
            } | WriteFact::Signing(SigningState::Refused { .. })
        )),
        "a relay-level rejection must never be reported as a routing failure; saw {seen:?}"
    );
    assert_eq!(
        relays_named_by(&seen),
        expected,
        "no relay outside the scope's own two named hosts was ever tried"
    );

    engine.shutdown();
    refusing.shutdown();
    accepting.shutdown();
}
