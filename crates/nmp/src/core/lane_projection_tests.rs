//! Falsifiers for the reducer-owned relay-worker projection (issue #985).
//!
//! These live in-crate rather than under `tests/` because the decisive claims
//! are about reducer-private state: `relay_worker_requirements` is
//! `pub(crate)`, and "the projection equals a fresh canonical-store
//! reconstruction" needs `PendingWrite::nonterminal_lane_relays` itself, not
//! an observable proxy for it.

use super::*;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crate::core::lane_projection::LaneProjection;
use nmp_router::FixtureDirectory;
use nmp_store::{
    ClaimSet, CompensationReason, CoverageInterval, GcReport, InsertOutcome, LaneDeadline,
    MemoryStore, PersistenceFault, RecoveredAttemptDetails, RecoveredIntent, RecoveredReceipt,
    RedbStore, RetractReason, StoredEvent,
};
use nostr::{Keys, Kind};

// ---- the enumeration falsifier -----------------------------------------

/// The structural requirement of #985: projection maintenance must not depend
/// on a contributor remembering to update a set at ~25 call sites, so a new
/// bypass has to fail MECHANICALLY.
///
/// Two halves, both derived from source rather than from a hand-kept list:
///
/// 1. **Nothing is missing from the enumeration.** Every `EventStore` door
///    that takes `&mut self` and deals in `RecoveredLane` is a lane-mutation
///    constructor; adding one to `nmp-store` without adding it to
///    [`LaneProjection::LANE_MUTATION_DOORS`] fails here. Two further
///    constructors do not mention `RecoveredLane` in their signature and are
///    therefore named explicitly: `close_terminal_intent` (removes an
///    intent's open work wholesale) and `record_route_revision`.
///
///    `record_route_revision` is the one the second #985 design comment
///    singles out. Today a revision mints no lane by itself — its paired
///    `bootstrap_outbox_lanes` does — so an enumeration written only against
///    today's call sites would look complete. Under #975 `Auto` re-executes
///    its strategy at EVERY send opportunity and appends a revision whenever
///    resolution learns something new, each of which mints lanes through this
///    projection's own door. Enumerating it now is what makes that future
///    path fail mechanically instead of silently.
///
/// 2. **Nothing bypasses the door.** No file in `crates/nmp/src` other than
///    `lane_projection.rs` may reach a lane-mutation door directly through
///    `store_mut()`. Whitespace is stripped before matching, so rustfmt's
///    multi-line `self\n.resolver\n.store_mut()\n.set_lane_waiting(` builder
///    chains are caught exactly like the single-line spelling.
#[test]
fn every_lane_mutation_constructor_goes_through_the_projection_door() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let store_src = manifest.join("../nmp-store/src/lib.rs");
    let store_source = std::fs::read_to_string(&store_src).expect("read nmp-store/src/lib.rs");

    // Half 1: derive the mutation doors from the store trait itself.
    let mut derived: Vec<String> = Vec::new();
    for chunk in store_source.split("\n    fn ").skip(1) {
        let name = chunk.split('(').next().unwrap_or_default().to_string();
        let signature = chunk.split('{').next().unwrap_or_default();
        if signature.contains("&mut self") && signature.contains("RecoveredLane") {
            derived.push(name);
        }
    }
    assert!(
        derived.len() >= 8,
        "the signature scrape found {} lane-mutation doors, which means the \
         scrape itself broke rather than the trait shrinking: {derived:?}",
        derived.len()
    );
    for door in &derived {
        assert!(
            LaneProjection::LANE_MUTATION_DOORS.contains(&door.as_str()),
            "`EventStore::{door}` mutates lane state but is not enumerated in \
             LANE_MUTATION_DOORS -- give it a `commit_*` door in \
             core/lane_projection.rs and list it there"
        );
    }
    for named in ["record_route_revision", "close_terminal_intent"] {
        assert!(
            LaneProjection::LANE_MUTATION_DOORS.contains(&named),
            "`{named}` is a lane-minting/removing constructor whose signature \
             does not mention RecoveredLane, so it must stay explicitly \
             enumerated"
        );
    }

    // Half 2: nobody reaches those doors around the projection.
    let mut offenders: Vec<String> = Vec::new();
    let mut files = Vec::new();
    collect_rust_sources(&manifest.join("src"), &mut files);
    assert!(
        files.len() > 10,
        "the source walk found only {} files; the walk broke, not the crate",
        files.len()
    );
    for file in files {
        if file.file_name().and_then(|n| n.to_str()) == Some("lane_projection.rs")
            || file.file_name().and_then(|n| n.to_str()) == Some("lane_projection_tests.rs")
        {
            continue;
        }
        let source = std::fs::read_to_string(&file).expect("read engine source");
        let squeezed: String = source.chars().filter(|c| !c.is_whitespace()).collect();
        for door in LaneProjection::LANE_MUTATION_DOORS {
            if squeezed.contains(&format!("store_mut().{door}(")) {
                offenders.push(format!("{} calls store_mut().{door}", file.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "every engine lane mutation must go through the reducer-owned \
         projection door in core/lane_projection.rs, so the committed \
         RecoveredLane can update `nonterminal_lane_relays`. Bypasses found: \
         {offenders:?}"
    );
}

fn collect_rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read engine source dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rust_sources(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

// ---- the behavioural falsifiers ----------------------------------------

/// The decisive #985 regression: once boot/acceptance has established `N`
/// pending intents, repeated UNCHANGED worker-demand passes must add exactly
/// zero `recover_outbox_lanes` calls. Before this change the count grew by
/// `N` on every single pass -- a redb range scan, a JSON decode per lane and
/// a `RelayUrl` reparse each time, all under the engine's serialization
/// boundary.
#[test]
fn unchanged_dispatch_passes_add_zero_lane_reads() {
    const N: usize = 4;
    let probe = LaneReadProbe::default();
    let mut core = probe_core(&probe);
    let mut authors = Vec::new();
    for i in 0..N {
        let author = Keys::generate();
        publish_to(&mut core, &author, &[relay(i)]);
        authors.push(author);
    }
    assert_eq!(
        core.pending.len(),
        N,
        "the fixture must establish N intents"
    );

    probe.reset();
    let mut sets = Vec::new();
    for _ in 0..5 {
        sets.push(
            core.relay_worker_requirements()
                .expect("worker demand is always answerable without a store read"),
        );
    }

    assert_eq!(
        probe.reads(),
        0,
        "five unchanged worker-demand passes over {N} pending intents must \
         perform ZERO recover_outbox_lanes calls; the old implementation \
         performed {}",
        5 * N
    );
    for pass in &sets {
        assert_eq!(
            pass.writes, sets[0].writes,
            "an unchanged reducer must answer identically every pass"
        );
    }
    assert_eq!(
        sets[0].writes.len(),
        N,
        "each of the {N} intents still owns exactly one nonterminal lane"
    );
}

/// The #968 parking property, stated now so it is testable before parking
/// itself lands: `N` intents that have never routed own no lane, so they
/// contribute zero worker demand and cost zero per-dispatch store reads.
///
/// An accepted-but-unrouted intent is exactly the shape `AwaitingRoute` will
/// have -- present in `pending`, owning a durable intent row, having minted
/// no lane -- so the property is asserted against the shape rather than
/// against a lifecycle that does not exist yet. When #968 lands, a parked
/// write that somehow acquired worker demand fails here.
#[test]
fn route_parked_intents_add_no_worker_demand_and_no_store_reads() {
    const PARKED: usize = 6;
    let probe = LaneReadProbe::default();
    let mut core = probe_core(&probe);

    // One ordinary routed write, so the assertions below distinguish "parked
    // writes contribute nothing" from "this core computes nothing at all".
    let routed_author = Keys::generate();
    publish_to(&mut core, &routed_author, &[relay(0)]);

    for i in 0..PARKED {
        let author = Keys::generate();
        accept_without_routing(&mut core, &author, 500 + i as u64);
    }
    assert_eq!(
        core.pending.len(),
        PARKED + 1,
        "every parked intent must still be a live durable obligation"
    );
    for pending in core.pending.values() {
        if pending.signing_pubkey == routed_author.public_key() {
            continue;
        }
        assert!(
            pending.lane_relays.is_empty() && pending.nonterminal_lane_relays.is_empty(),
            "a write that never routed has minted no lane"
        );
    }

    probe.reset();
    let before = core
        .relay_worker_requirements()
        .expect("worker demand")
        .writes;
    for _ in 0..5 {
        let pass = core
            .relay_worker_requirements()
            .expect("worker demand")
            .writes;
        assert_eq!(pass, before, "parked intents cannot perturb worker demand");
    }
    assert_eq!(
        probe.reads(),
        0,
        "{PARKED} parked intents must cost zero per-dispatch store reads"
    );
    assert_eq!(
        before.len(),
        1,
        "only the one routed write owns a worker; {PARKED} parked intents add \
         nothing, got {before:?}"
    );
}

/// Every committed transition must leave the projection equal to a fresh
/// canonical-store reconstruction -- the old `write_relay_workers` body,
/// recomputed from `recover_outbox_lanes` on demand. Driven through a real
/// lifecycle: bootstrap, waiting connection, eligible, in-flight, transient
/// retry, re-eligible, in-flight again, and finally a terminal ack.
#[test]
fn every_committed_transition_agrees_with_a_canonical_rebuild() {
    let author = Keys::generate();
    let url = relay(0);
    let probe = LaneReadProbe::default();
    let mut core = probe_core(&probe);
    let session = RelaySessionKey::new(url.clone(), AccessContext::Nip42(author.public_key()));

    let mut checkpoints: Vec<&str> = Vec::new();
    let check = |core: &EngineCore<ProbeStore<MemoryStore>>, label: &'static str| {
        assert_eq!(
            core.write_relay_workers().expect("projected workers"),
            canonical_write_relay_workers(core),
            "projection disagrees with a fresh canonical-store reconstruction \
             after: {label}"
        );
    };

    core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
    check(&core, "activation");
    checkpoints.push("activation");

    // Signed but unconnected -> the lane bootstraps into WaitingConnection.
    let effects = publish_to(&mut core, &author, std::slice::from_ref(&url));
    check(&core, "bootstrap into WaitingConnection");
    checkpoints.push("bootstrap");
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::EnsureWriteRelay(s) if s == &session)),
        "the bootstrapped lane must demand its own signer session"
    );

    // Connect + release the AUTH probe -> Eligible -> InFlight.
    core.handle(EngineMsg::RelayConnected(
        TransportRelayHandle {
            slot: 0,
            generation: 1,
        },
        session.clone(),
    ));
    check(&core, "relay connected");
    checkpoints.push("connected");
    let effects = core.handle(EngineMsg::AuthProbeReleased(
        TransportRelayHandle {
            slot: 0,
            generation: 1,
        },
        session.clone(),
    ));
    check(&core, "auth probe released (eligible -> in flight)");
    checkpoints.push("in-flight");

    let correlation = effects
        .iter()
        .find_map(|e| match e {
            Effect::PublishEvent(_, _, correlation) => Some(*correlation),
            _ => None,
        })
        .expect("the woken lane publishes");
    let event_id = core
        .pending
        .values()
        .find_map(|p| p.event_id)
        .expect("the signed event id");

    core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
    check(&core, "handoff written (awaiting ack)");
    checkpoints.push("awaiting-ack");

    // A transient rejection: nonterminal, so the worker stays owned.
    core.handle(EngineMsg::RelayFrame(
        TransportRelayHandle {
            slot: 0,
            generation: 1,
        },
        session.clone(),
        RelayFrame::from(RelayMessage::ok(event_id, false, "rate-limited: slow down")),
    ));
    check(&core, "transient rejection (retry eligible)");
    checkpoints.push("transient");
    assert!(
        core.write_relay_workers()
            .expect("workers")
            .contains(&session),
        "a transient retry is nonterminal -- the worker must be retained"
    );

    // Disconnect + reconnect drives waiting/eligible again.
    core.handle(EngineMsg::RelayDisconnected(
        TransportRelayHandle {
            slot: 0,
            generation: 1,
        },
        session.clone(),
        DisconnectReason::Error,
    ));
    check(&core, "disconnect");
    checkpoints.push("disconnect");

    // Terminal ack: the ONLY thing that may retract worker demand.
    core.handle(EngineMsg::RelayConnected(
        TransportRelayHandle {
            slot: 1,
            generation: 2,
        },
        session.clone(),
    ));
    core.handle(EngineMsg::AuthProbeReleased(
        TransportRelayHandle {
            slot: 1,
            generation: 2,
        },
        session.clone(),
    ));
    check(&core, "reconnected");
    checkpoints.push("reconnected");
    // The transient retry is deadline-driven: only a due `RetryEligible`
    // deadline moves it back to `Eligible` and lets `schedule_ready` mint the
    // next attempt.
    let effects = core.handle(EngineMsg::Tick(Timestamp::from(10_000u64)));
    check(
        &core,
        "retry deadline due (transient -> eligible -> in flight)",
    );
    checkpoints.push("rescheduled");
    let correlation = effects
        .iter()
        .find_map(|e| match e {
            Effect::PublishEvent(_, _, correlation) => Some(*correlation),
            _ => None,
        })
        .expect("the retry publishes");
    core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
    check(&core, "retry handed off");
    core.handle(EngineMsg::RelayFrame(
        TransportRelayHandle {
            slot: 1,
            generation: 2,
        },
        session.clone(),
        RelayFrame::from(RelayMessage::ok(event_id, true, "")),
    ));
    check(&core, "terminal ack");
    checkpoints.push("acked");

    assert!(
        !core
            .write_relay_workers()
            .expect("workers")
            .contains(&session),
        "a committed terminal lane must retract its worker demand"
    );
    assert!(
        core.pending.is_empty(),
        "the all-terminal intent closes without a preceding lane scan"
    );
    assert!(
        checkpoints.len() >= 8,
        "the lifecycle must actually reach every checkpoint: {checkpoints:?}"
    );
}

/// Exact relay PLUS access identity: two different signing identities
/// publishing to the SAME relay URL must project to two distinct
/// `RelaySessionKey`s and must retire independently. A projection keyed by
/// URL alone would collapse them and silently drop a still-required worker
/// when the first one terminates.
#[test]
fn separate_access_identities_at_one_relay_url_stay_distinct() {
    let url = relay(0);
    let author_a = Keys::generate();
    let author_b = Keys::generate();
    let probe = LaneReadProbe::default();
    let mut core = probe_core(&probe);
    let session_a = RelaySessionKey::new(url.clone(), AccessContext::Nip42(author_a.public_key()));
    let session_b = RelaySessionKey::new(url.clone(), AccessContext::Nip42(author_b.public_key()));
    assert_ne!(session_a, session_b);

    publish_to(&mut core, &author_a, std::slice::from_ref(&url));
    publish_to(&mut core, &author_b, std::slice::from_ref(&url));

    let workers = core.write_relay_workers().expect("workers");
    assert!(
        workers.contains(&session_a) && workers.contains(&session_b),
        "one URL under two signing identities is two workers, got {workers:?}"
    );
    assert!(
        !workers.contains(&RelaySessionKey::new(url.clone(), AccessContext::Public)),
        "a write never projects onto the relay's Public read session"
    );
    assert_eq!(
        workers,
        canonical_write_relay_workers(&core),
        "identity-scoped projection must equal the canonical reconstruction"
    );

    // Drive ONLY author A's lane to a committed terminal ack, through the
    // ordinary engine path. B never connects, so its lane stays
    // `WaitingConnection` at the very same URL.
    let event_a = core
        .pending
        .values()
        .find(|p| p.signing_pubkey == author_a.public_key())
        .and_then(|p| p.event_id)
        .expect("author A's write is signed");
    let handle = TransportRelayHandle {
        slot: 0,
        generation: 1,
    };
    core.handle(EngineMsg::RelayConnected(handle, session_a.clone()));
    let effects = core.handle(EngineMsg::AuthProbeReleased(handle, session_a.clone()));
    let correlation = effects
        .iter()
        .find_map(|e| match e {
            Effect::PublishEvent(target, _, correlation) if target == &session_a => {
                Some(*correlation)
            }
            _ => None,
        })
        .expect("only author A's lane wakes on author A's session");
    core.handle(EngineMsg::EventHandoff(correlation, HandoffResult::Written));
    core.handle(EngineMsg::RelayFrame(
        handle,
        session_a.clone(),
        RelayFrame::from(RelayMessage::ok(event_a, true, "")),
    ));

    let workers = core.write_relay_workers().expect("workers");
    assert!(
        !workers.contains(&session_a),
        "author A's terminal lane must retract exactly its own worker"
    );
    assert!(
        workers.contains(&session_b),
        "author B's still-nonterminal lane at the SAME URL must be untouched -- \
         a URL-keyed projection would have dropped it here, got {workers:?}"
    );
    assert_eq!(workers, canonical_write_relay_workers(&core));
}

/// Close and reopen a real `RedbStore`: the reconstructed projection and the
/// ordered worker requirements must equal the pre-close durable meaning, and
/// the reconstruction itself must be the ONE lane recovery per open intent --
/// after it, unchanged passes read nothing.
#[test]
fn reopen_reconstructs_the_same_worker_set_without_further_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("worker-projection.redb");
    let author_a = Keys::generate();
    let author_b = Keys::generate();
    let shared = relay(0);
    let solo = relay(1);

    let before = {
        let probe = LaneReadProbe::default();
        let mut core = EngineCore::new(
            ProbeStore::new(RedbStore::open(&path).unwrap(), probe.clone()),
            Box::new(FixtureDirectory::new()),
            10,
        );
        publish_to(&mut core, &author_a, &[shared.clone(), solo.clone()]);
        publish_to(&mut core, &author_b, std::slice::from_ref(&shared));
        let workers = core.write_relay_workers().expect("workers");
        assert_eq!(workers, canonical_write_relay_workers(&core));
        workers
    };
    assert_eq!(
        before.len(),
        3,
        "two intents, three lanes, two access identities: {before:?}"
    );

    let probe = LaneReadProbe::default();
    let mut core = EngineCore::new(
        ProbeStore::new(RedbStore::open(&path).unwrap(), probe.clone()),
        Box::new(FixtureDirectory::new()),
        10,
    );
    core.recover_on_boot();
    let recovered = core.write_relay_workers().expect("workers");
    assert_eq!(
        recovered, before,
        "reopen must reconstruct the exact pre-close worker set, including \
         both access identities at {shared}"
    );
    assert_eq!(recovered, canonical_write_relay_workers(&core));
    assert!(
        !core.worker_projection_degraded,
        "a clean reconstruction discharges any degradation"
    );

    probe.reset();
    for _ in 0..5 {
        assert_eq!(core.write_relay_workers().expect("workers"), recovered);
    }
    assert_eq!(
        probe.reads(),
        0,
        "after boot reconstruction, unchanged worker-demand passes read nothing"
    );
}

/// A store mutation that provably did not commit (`DurabilityOutcome::
/// Absent`) must leave the projection exactly as it was: the pre-transition
/// state is still the durable truth, so "fail closed" here means "do not
/// move", not "widen".
#[test]
fn an_absent_failure_leaves_the_projection_unchanged() {
    let author = Keys::generate();
    let url = relay(0);
    let probe = LaneReadProbe::default();
    let mut core = probe_core(&probe);
    publish_to(&mut core, &author, std::slice::from_ref(&url));

    let before = core.write_relay_workers().expect("workers");
    let intent = core
        .pending
        .values()
        .find_map(|p| p.intent_id)
        .expect("durable intent");
    let lane = core
        .resolver
        .store()
        .recover_outbox_lanes(intent)
        .expect("lanes")
        .remove(0);

    // `Invariant` is the classification for "this crate refused its own
    // input"; #904 proves it absent, because decoding always precedes the
    // enclosing write transaction's commit.
    core.resolver
        .store_mut()
        .arm(LaneFault::Finish(PersistenceFault::Invariant));
    let err = core
        .commit_lane_finish(
            &lane.key,
            lane.revision,
            0,
            AttemptOutcome::Acked,
            Timestamp::from(50u64),
        )
        .expect_err("the injected failure surfaces");
    assert_eq!(err.durability(), DurabilityOutcome::Absent);

    assert_eq!(
        core.write_relay_workers().expect("workers"),
        before,
        "an absent terminal transition must not retract a worker"
    );
    assert!(
        !core.worker_projection_degraded,
        "a provably-absent failure says nothing is unknown"
    );
    assert_eq!(
        core.write_relay_workers().expect("workers"),
        canonical_write_relay_workers(&core),
        "and the projection still equals the durable truth"
    );
}

/// A terminal transition whose durability is UNKNOWN (#904 `Io` /
/// `Corrupted` / `LockPoisoned`) may or may not have landed. Retracting the
/// worker would drop a session a still-open lane needs, so the projection
/// retains it and latches `worker_projection_degraded` instead -- and
/// crucially does NOT start scanning again.
#[test]
fn an_unknown_terminal_failure_retains_the_worker_and_never_rescans() {
    let author = Keys::generate();
    let url = relay(0);
    let probe = LaneReadProbe::default();
    let mut core = probe_core(&probe);
    publish_to(&mut core, &author, std::slice::from_ref(&url));
    let before = core.write_relay_workers().expect("workers");

    let intent = core
        .pending
        .values()
        .find_map(|p| p.intent_id)
        .expect("durable intent");
    let lane = core
        .resolver
        .store()
        .recover_outbox_lanes(intent)
        .expect("lanes")
        .remove(0);

    core.resolver
        .store_mut()
        .arm(LaneFault::Finish(PersistenceFault::Io));
    let err = core
        .commit_lane_finish(
            &lane.key,
            lane.revision,
            0,
            AttemptOutcome::Acked,
            Timestamp::from(50u64),
        )
        .expect_err("the injected failure surfaces");
    assert_eq!(err.durability(), DurabilityOutcome::Unknown);

    assert!(
        core.worker_projection_degraded,
        "an unknown outcome must mark the projection degraded"
    );
    probe.reset();
    for _ in 0..5 {
        assert_eq!(
            core.write_relay_workers().expect("workers"),
            before,
            "an unknown outcome retains every possibly-required worker"
        );
    }
    assert_eq!(
        probe.reads(),
        0,
        "degradation must never become a per-dispatch scan -- that is the \
         exact defect #985 removes"
    );
}

/// The hardest failure shape from the design doc: a lane-CREATION whose
/// durability is unknown. Retaining the old projection is not enough, because
/// the possibly-durable lane is NEW. Every relay the attempted bootstrap could
/// have minted a lane for must be conservatively added.
#[test]
fn an_unknown_lane_creation_failure_retains_every_candidate_worker() {
    let author = Keys::generate();
    let urls = [relay(0), relay(1), relay(2)];
    let probe = LaneReadProbe::default();
    let mut core = probe_core(&probe);
    core.resolver
        .store_mut()
        .arm(LaneFault::Bootstrap(PersistenceFault::Io));
    publish_to(&mut core, &author, &urls);

    assert!(
        core.worker_projection_degraded,
        "an unknown bootstrap outcome must mark the projection degraded"
    );
    let workers = core.write_relay_workers().expect("workers");
    for url in &urls {
        assert!(
            workers.contains(&RelaySessionKey::new(
                url.clone(),
                AccessContext::Nip42(author.public_key())
            )),
            "every relay the failed bootstrap could have minted a lane for must \
             be conservatively retained, {url} is missing from {workers:?}"
        );
    }

    probe.reset();
    for _ in 0..5 {
        assert_eq!(core.write_relay_workers().expect("workers"), workers);
    }
    assert_eq!(
        probe.reads(),
        0,
        "still no per-dispatch scan while degraded"
    );
}

// ---- fixtures ----------------------------------------------------------

fn relay(i: usize) -> RelayUrl {
    RelayUrl::parse(&format!("wss://worker-projection-{i}.example.com")).unwrap()
}

#[derive(Clone, Default)]
struct SilentSink;

impl ReceiptSink for SilentSink {
    fn on_status(&self, _status: WriteStatus) -> bool {
        true
    }
}

fn probe_core(probe: &LaneReadProbe) -> EngineCore<ProbeStore<MemoryStore>> {
    EngineCore::new(
        ProbeStore::new(MemoryStore::new(), probe.clone()),
        Box::new(FixtureDirectory::new()),
        10,
    )
}

fn intent_for(author: &Keys, seq: u64, relays: &[RelayUrl]) -> WriteIntent {
    WriteIntent {
        payload: WritePayload::Unsigned(UnsignedEvent::new(
            author.public_key(),
            Timestamp::from(seq),
            Kind::TextNote,
            Vec::new(),
            format!("worker projection {seq}"),
        )),
        durability: Durability::Durable,
        routing: WriteRouting::PrivateNarrow(PrivateRoute {
            relays: NarrowOnly::new(relays.to_vec()),
        }),
        identity_override: None,
        correlation: None,
    }
}

/// Accept + sign one durable private write, which routes and bootstraps its
/// lanes. Returns the `on_signed` effects.
fn publish_to<S: EventStore>(
    core: &mut EngineCore<S>,
    author: &Keys,
    relays: &[RelayUrl],
) -> Vec<Effect> {
    core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
    let accepted = core.handle(EngineMsg::Publish(
        intent_for(author, 100, relays),
        Box::new(SilentSink),
    ));
    let (id, generation, unsigned) = accepted
        .iter()
        .find_map(|e| match e {
            Effect::RequestSign(id, generation, unsigned) => {
                Some((*id, *generation, unsigned.clone()))
            }
            _ => None,
        })
        .expect("acceptance requests a signature");
    let signed = unsigned.sign_with_keys(author).expect("sign fixture");
    core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)))
}

/// Accept a durable write and stop: no signature, so no route resolution and
/// no lane. This is the shape #968's `AwaitingRoute` parking has -- a live
/// durable obligation in `pending` that has never minted a lane.
fn accept_without_routing<S: EventStore>(core: &mut EngineCore<S>, author: &Keys, seq: u64) {
    core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));
    let accepted = core.handle(EngineMsg::Publish(
        intent_for(author, seq, &[relay(9)]),
        Box::new(SilentSink),
    ));
    assert!(
        accepted
            .iter()
            .any(|e| matches!(e, Effect::RequestSign(..))),
        "the parked-shape fixture must actually be accepted"
    );
}

/// The OLD `write_relay_workers` body, kept verbatim as the oracle: rebuild
/// worker demand from canonical store rows on demand. Every projection
/// assertion above is "the projection equals what a fresh reconstruction
/// would say".
fn canonical_write_relay_workers<S: EventStore>(core: &EngineCore<S>) -> BTreeSet<RelaySessionKey> {
    let mut required: BTreeSet<RelaySessionKey> = core
        .attempt_correlations
        .values()
        .map(|target| target.session.clone())
        .collect();
    for pending in core.pending.values() {
        let access = AccessContext::Nip42(pending.signing_pubkey);
        required.extend(
            pending
                .pending_relays
                .iter()
                .chain(&pending.unstarted_relays)
                .chain(&pending.route_blocked_relays)
                .cloned()
                .map(|relay| RelaySessionKey::new(relay, access)),
        );
        let Some(intent_id) = pending.intent_id else {
            continue;
        };
        let lanes = core
            .resolver
            .store()
            .recover_outbox_lanes(intent_id)
            .expect("canonical reconstruction reads lanes");
        required.extend(lanes.into_iter().filter_map(|lane| {
            (!matches!(lane.state, LaneState::Terminal { .. }))
                .then_some(RelaySessionKey::new(lane.key.relay, access))
        }));
    }
    required
}

// ---- the instrumented store double -------------------------------------

/// Shared `recover_outbox_lanes` counter, readable after the store has been
/// moved into `EngineCore`.
#[derive(Clone, Default)]
struct LaneReadProbe(Rc<Cell<u64>>);

impl LaneReadProbe {
    fn reads(&self) -> u64 {
        self.0.get()
    }
    fn reset(&self) {
        self.0.set(0);
    }
}

/// Which lane door to fail, and with which #904 fault classification.
#[derive(Clone, Copy)]
enum LaneFault {
    Bootstrap(PersistenceFault),
    Finish(PersistenceFault),
}

/// Counts lane reads and can fail one lane door exactly once. Generic over
/// the backend so the same falsifiers run against `MemoryStore` and a real
/// on-disk `RedbStore`.
struct ProbeStore<S: EventStore> {
    inner: S,
    probe: LaneReadProbe,
    armed: Option<LaneFault>,
}

impl<S: EventStore> ProbeStore<S> {
    fn new(inner: S, probe: LaneReadProbe) -> Self {
        Self {
            inner,
            probe,
            armed: None,
        }
    }

    fn arm(&mut self, fault: LaneFault) {
        self.armed = Some(fault);
    }

    fn take_fault(&mut self, want_bootstrap: bool) -> Option<PersistenceError> {
        let fault = match (self.armed, want_bootstrap) {
            (Some(LaneFault::Bootstrap(fault)), true) => fault,
            (Some(LaneFault::Finish(fault)), false) => fault,
            _ => return None,
        };
        self.armed = None;
        Some(PersistenceError::new(fault, "injected lane-door failure"))
    }
}

impl<S: EventStore> EventStore for ProbeStore<S> {
    fn insert(
        &mut self,
        event: SignedEvent,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        self.inner.insert(event, from)
    }
    fn query(&self, filter: &nostr::Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.query(filter)
    }
    fn remove(
        &mut self,
        id: EventId,
        reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        self.inner.remove(id, reason)
    }
    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.expire_due(now)
    }
    fn next_expiration(&self) -> Option<Timestamp> {
        self.inner.next_expiration()
    }
    fn record_coverage(
        &mut self,
        atom: &ContextualAtom,
        relay: &RelayUrl,
        proven: CoverageInterval,
    ) -> Result<(), PersistenceError> {
        self.inner.record_coverage(atom, relay, proven)
    }
    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
        self.inner.get_coverage(key, relay)
    }
    fn gc(&mut self, claims: &ClaimSet) -> Result<GcReport, PersistenceError> {
        self.inner.gc(claims)
    }
    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        self.inner.accept_write(accept)
    }
    fn promote_signed(
        &mut self,
        intent_id: IntentId,
        sig: nostr::secp256k1::schnorr::Signature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        self.inner.promote_signed(intent_id, sig)
    }
    fn cancel_ephemeral_receipt(
        &mut self,
        receipt_id: u64,
    ) -> Result<CancelEphemeralOutcome, PersistenceError> {
        self.inner.cancel_ephemeral_receipt(receipt_id)
    }
    fn mark_ephemeral_signed(&mut self, receipt_id: u64) -> Result<bool, PersistenceError> {
        self.inner.mark_ephemeral_signed(receipt_id)
    }
    fn compensate_write_with_state(
        &mut self,
        intent_id: IntentId,
        reason: CompensationReason,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write_with_state(intent_id, reason)
    }
    fn recover_outbox(&self) -> Result<Vec<RecoveredIntent>, PersistenceError> {
        self.inner.recover_outbox()
    }
    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<RecoveredReceipt>, PersistenceError> {
        self.inner.reattach_receipt(receipt_id)
    }
    fn lookup_correlation(&self, token: &str) -> Result<Option<u64>, PersistenceError> {
        self.inner.lookup_correlation(token)
    }
    fn record_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<RecoveredRouteRevision, PersistenceError> {
        self.inner.record_route_revision(intent_id, relays)
    }
    fn recover_route_revisions(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredRouteRevision>, PersistenceError> {
        self.inner.recover_route_revisions(intent_id)
    }
    fn recover_attempts(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
    }
    fn bootstrap_outbox_lanes(
        &mut self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredLane>, PersistenceError> {
        if let Some(err) = self.take_fault(true) {
            return Err(err);
        }
        self.inner.bootstrap_outbox_lanes(intent_id)
    }
    fn recover_outbox_lanes(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredLane>, PersistenceError> {
        self.probe.0.set(self.probe.0.get() + 1);
        self.inner.recover_outbox_lanes(intent_id)
    }
    fn due_outbox_deadlines(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<LaneDeadline>, PersistenceError> {
        self.inner.due_outbox_deadlines(now, limit)
    }
    fn next_outbox_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        self.inner.next_outbox_deadline()
    }
    fn set_lane_waiting(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        auth: bool,
    ) -> Result<RecoveredLane, PersistenceError> {
        self.inner.set_lane_waiting(key, expected_revision, auth)
    }
    fn set_lane_eligible(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        since: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
        self.inner.set_lane_eligible(key, expected_revision, since)
    }
    fn set_lane_transient(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        eligible_at: Timestamp,
        cause: TransientCause,
        raw_reason: Option<String>,
    ) -> Result<RecoveredLane, PersistenceError> {
        self.inner.set_lane_transient(
            key,
            expected_revision,
            ordinal,
            eligible_at,
            cause,
            raw_reason,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn suspend_lane_attempt(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        at: Timestamp,
        cause: TransientCause,
        raw_reason: Option<String>,
        auth: bool,
    ) -> Result<RecoveredLane, PersistenceError> {
        self.inner.suspend_lane_attempt(
            key,
            expected_revision,
            ordinal,
            at,
            cause,
            raw_reason,
            auth,
        )
    }
    fn start_lane_attempt(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        event: SignedEvent,
        started_at: Timestamp,
    ) -> Result<(RecoveredAttempt, RecoveredLane), PersistenceError> {
        self.inner
            .start_lane_attempt(key, expected_revision, event, started_at)
    }
    fn record_lane_handoff(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        detail: AttemptHandoffDetail,
        next: PostHandoffState,
    ) -> Result<RecoveredLane, PersistenceError> {
        self.inner
            .record_lane_handoff(key, expected_revision, ordinal, detail, next)
    }
    fn finish_lane_attempt(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        outcome: AttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
        if let Some(err) = self.take_fault(false) {
            return Err(err);
        }
        self.inner
            .finish_lane_attempt(key, expected_revision, ordinal, outcome, finished_at)
    }
    fn recover_attempt_details(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<RecoveredAttemptDetails>, PersistenceError> {
        self.inner.recover_attempt_details(intent_id)
    }
    fn close_terminal_intent(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        self.inner.close_terminal_intent(intent_id)
    }
    fn accept_ephemeral(
        &mut self,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
    ) -> Result<u64, PersistenceError> {
        self.inner.accept_ephemeral(frozen_id, expected_pubkey)
    }
}

// Silence the unused-import lint if a future edit drops one of these.
#[allow(dead_code)]
fn _assert_helpers_used() {
    let _: Option<Arc<Mutex<()>>> = None;
}

/// The reproducible before/after for #985's own claim, run on demand:
///
/// ```text
/// cargo test --release -p nmp --lib measure_worker_demand_cost -- --ignored --nocapture
/// ```
///
/// It measures BOTH bodies against the SAME populated `RedbStore` in the same
/// process, so the comparison is not across builds: `write_relay_workers` (the
/// projection) versus `canonical_write_relay_workers` (the old body, kept
/// verbatim as this file's oracle). `#[ignore]`d because it builds a real
/// on-disk store with hundreds of intents and reports a wall-clock number,
/// neither of which belongs in the ordinary suite.
///
/// This is NOT the Mosaico-shaped end-to-end profile the issue also asks for;
/// it measures exactly the one calculation this change replaces. Whole-process
/// CPU remains unmeasured here and must not be claimed from this number.
#[test]
#[ignore]
fn measure_worker_demand_cost() {
    use std::time::Instant;
    const N: usize = 200;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("measure.redb");
    let probe = LaneReadProbe::default();
    let mut core = EngineCore::new(
        ProbeStore::new(RedbStore::open(&path).unwrap(), probe.clone()),
        Box::new(FixtureDirectory::new()),
        10,
    );
    for i in 0..N {
        let author = Keys::generate();
        publish_to(
            &mut core,
            &author,
            &[relay(i * 3), relay(i * 3 + 1), relay(i * 3 + 2)],
        );
    }
    let passes = 500;

    probe.reset();
    let t = Instant::now();
    let mut n = 0usize;
    for _ in 0..passes {
        n += core.write_relay_workers().unwrap().len();
    }
    let after = t.elapsed();
    let after_reads = probe.reads();

    probe.reset();
    let t = Instant::now();
    let mut m = 0usize;
    for _ in 0..passes {
        m += canonical_write_relay_workers(&core).len();
    }
    let before = t.elapsed();
    let before_reads = probe.reads();

    assert_eq!(n, m);
    println!(
        "N={N} intents x 3 relays, {passes} worker-demand passes (RedbStore)\n  BEFORE (old body, per-intent recover_outbox_lanes): {before:?}, {before_reads} lane reads\n  AFTER  (reducer projection): {after:?}, {after_reads} lane reads\n  speedup: {:.1}x",
        before.as_secs_f64() / after.as_secs_f64()
    );
}
