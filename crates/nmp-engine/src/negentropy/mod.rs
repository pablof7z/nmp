//! Prober FSM + `ProbedRelay` capability token + the `Reconciler` wrapper
//! around the external `::negentropy` set-reconciliation crate (plan §3.4,
//! §1 "module not a crate": reducer-coupled, driven turn-by-turn by
//! `EngineCore`, sharing its message vocabulary). HARVEST target:
//! `crates/nmp-nip77/src/{runtime,reconciler,filter,messages,codec}.rs` in
//! the old repo — the `RelayNegentropyState` FSM (-> [`ProbeState`] here),
//! `EligibleFilter` parse, `Reconciler` over the `negentropy` crate, and
//! the 30s liveness-deadline REQ fallback are re-justified here (plan
//! §4). The kernel "substrate seam" (outbound-REQ/inbound-text
//! interceptor) is DROPPED — `EngineCore`'s reducer (`core/mod.rs`) drives
//! the prober and every reconciliation round directly, via `on_relay_frame`
//! and `recompile`, exactly the way it already drives REQ/EOSE.
//!
//! **Naming gotcha for E:** this module is named `negentropy`, the same
//! name as the external `negentropy` crate this workspace depends on
//! (`nmp-engine/Cargo.toml`). A local `mod negentropy` shadows the
//! extern-prelude name at the crate root, so bare `negentropy::Foo` inside
//! this crate resolves to THIS module, not the external crate. Refer to
//! the external crate as `::negentropy::Foo` (leading `::`) wherever E
//! wraps its `Reconciler`.
//!
//! ## Ledger #8, structural (not a runtime `if`)
//!
//! [`ProbedRelay`]'s inner field is `pub(crate)` and this module hands out
//! NO public constructor for it: the ONLY place a `ProbedRelay` is ever
//! created is [`Prober::probed`] (reading a cached `Supported` verdict) and
//! [`Prober::on_neg_msg`] (a probe response arriving). `core::Effect::
//! NegOpen`'s first field is `ProbedRelay`, never `RelayUrl` — a caller
//! holding only a bare `RelayUrl` structurally cannot construct the
//! argument `NegOpen` requires; there is no widen/coerce path from
//! `RelayUrl` to `ProbedRelay` anywhere in this crate. An unprobed relay's
//! demand therefore falls through to a plain REQ *by construction*, not by
//! a `Prober::state(..) == Supported` check a future edit could accidentally
//! invert or bypass.
//!
//! ## Coverage from NEG-DONE
//!
//! There is no `NEG-DONE` wire frame in NIP-77 — the CLIENT detects
//! completion locally (`::negentropy::Negentropy::reconcile_with_ids`
//! returning `None`: no further query to send). [`Reconciler::step`]
//! surfaces that moment as [`NegStep::Done`]; `core::mod`'s
//! `finish_neg_session` then attributes coverage through the EXACT SAME
//! `AttributionState::attribute_eose` call the real EOSE path already
//! uses (`docs/consults/2026-07-11-fable-coverage-attribution.md`: "attribute
//! coverage via the same absorbed-snapshot mechanism EngineCore already
//! uses for EOSE; feed a NEG-DONE the same way") — this module never
//! invents a second coverage mechanism, it only drives the protocol and
//! hands `core::mod` a `BTreeSet<EventId>` of what the local store is
//! still missing.

use std::collections::{BTreeSet, HashMap};

use nmp_grammar::{AccessContext, ConcreteFilter, SourceAuthority};
use nmp_router::SubId;
use nostr::{EventId, RelayUrl};

use ::negentropy::{Id as NegId, Negentropy, NegentropyStorageVector};

/// = the old repo's `RelayNegentropyState`, re-cut for the new reducer
/// vocabulary.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ProbeState {
    Unknown,
    Probing,
    Supported,
    Unsupported,
}

/// Capability TOKEN (ledger #8): constructible ONLY from a `Supported`
/// cache entry (see the module doc's "structural, not a runtime `if`"
/// section). `NegOpen`/negentropy-sync effects take a `ProbedRelay`, never
/// a bare `RelayUrl` — an unprobed relay cannot reach the negentropy path;
/// it gets a plain REQ instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedRelay(pub(crate) RelayUrl);

impl ProbedRelay {
    /// The relay this token proves NIP-77 support for.
    pub fn url(&self) -> &RelayUrl {
        &self.0
    }
}

/// The minimal, throwaway filter a capability probe reconciles against —
/// scoped to kind:0 metadata with a tiny cap so probing a relay never
/// pulls (or asks the relay to compute over) anything resembling its whole
/// database; the probe only needs to observe SOME `NEG-MSG`/`NEG-ERR`
/// response, its content is never inspected.
fn probe_filter() -> ConcreteFilter {
    ConcreteFilter {
        kinds: Some(BTreeSet::from([0u16])),
        limit: Some(1),
        ..ConcreteFilter::default()
    }
}

/// Everything the runtime needs to place a probing `NEG-OPEN` frame on the
/// wire. `core::Effect::StartProbe` carries these fields (rather than a
/// bare `RelayUrl`) because the runtime (`runtime/mod.rs`) is a pure effect
/// dispatcher with no negentropy-protocol knowledge of its own — the
/// sub-id/filter/initial message are THIS module's decision, not the
/// runtime's.
pub struct ProbeRequest {
    pub sub_id: SubId,
    pub filter: ConcreteFilter,
    pub initial_message_hex: String,
}

/// Per-relay negentropy-support cache the prober FSM maintains, plus the
/// bookkeeping needed to attribute an inbound `NEG-MSG`/`NEG-ERR` back to
/// the specific probe that sent it (a probe's wire sub-id is never reused
/// for anything else, and lives in ITS OWN map here — entirely separate
/// from `core::attribution`'s bookkeeping for real REQ/negentropy
/// sessions, so the two can never be confused for one another).
pub struct Prober {
    pub states: HashMap<RelayUrl, ProbeState>,
    pending: HashMap<(RelayUrl, String), SubId>,
}

impl Default for Prober {
    fn default() -> Self {
        Self::new()
    }
}

impl Prober {
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
            pending: HashMap::new(),
        }
    }

    pub fn state(&self, relay: &RelayUrl) -> ProbeState {
        self.states
            .get(relay)
            .copied()
            .unwrap_or(ProbeState::Unknown)
    }

    /// The capability TOKEN (ledger #8): `Some` iff `relay` has been proven
    /// to support NIP-77. The ONLY function in this crate that constructs a
    /// `ProbedRelay` from an already-cached verdict (see [`Self::on_neg_msg`]
    /// for the other, "just learned it" construction site).
    pub fn probed(&self, relay: &RelayUrl) -> Option<ProbedRelay> {
        (self.state(relay) == ProbeState::Supported).then(|| ProbedRelay(relay.clone()))
    }

    /// Begin probing `relay` if its state is `Unknown` (idempotent — a
    /// relay already `Probing`/`Supported`/`Unsupported` is a cached
    /// verdict, never re-probed on this connection). Returns the wire
    /// request for the runtime to place, or `None` if there is nothing new
    /// to do.
    pub fn begin_probe(&mut self, relay: &RelayUrl) -> Option<ProbeRequest> {
        if self.state(relay) != ProbeState::Unknown {
            return None;
        }
        self.states.insert(relay.clone(), ProbeState::Probing);

        let filter = probe_filter();
        // A protocol-support probe, never a real acquisition (#106): its
        // sub-id lives in this module's OWN `pending` map, entirely
        // separate from `core::attribution`'s bookkeeping, so a fixed
        // context is harmless -- it never touches coverage/attribution
        // identity at all.
        let sub_id = SubId::for_wire(
            relay.clone(),
            &filter,
            &SourceAuthority::Public,
            AccessContext::Public,
        );
        let wire_id = crate::core::wire_sub_id_string(&sub_id);

        // An empty, sealed storage: a probe measures PROTOCOL support, not
        // whether local/remote ids agree — the have/need sets a probe's own
        // reconciliation would produce are never read back (see
        // `on_neg_msg`/`on_neg_unsupported`, neither inspects the payload).
        let mut storage = NegentropyStorageVector::new();
        storage
            .seal()
            .expect("a freshly-constructed, empty storage always seals cleanly");
        let mut probe =
            Negentropy::owned(storage, 0).expect("frame_size_limit=0 (unlimited) is always valid");
        let initial = probe
            .initiate()
            .expect("a fresh Negentropy has never built an initial message before this call");

        self.pending
            .insert((relay.clone(), wire_id), sub_id.clone());
        Some(ProbeRequest {
            sub_id,
            filter,
            initial_message_hex: hex::encode(initial),
        })
    }

    /// An inbound `NEG-MSG` for `wire_sub_id` on `relay`: if this was one of
    /// THIS prober's own pending probes, the relay understood `NEG-OPEN` —
    /// classify `Supported` and hand back the token that unlocks the real
    /// negentropy sync path. Returns `None` for any other sub (a live
    /// reconciliation session, handled by `EngineCore` directly against its
    /// own attribution bookkeeping — see `core::mod`'s `on_relay_frame`).
    pub fn on_neg_msg(&mut self, relay: &RelayUrl, wire_sub_id: &str) -> Option<ProbedRelay> {
        self.pending
            .remove(&(relay.clone(), wire_sub_id.to_string()))?;
        self.states.insert(relay.clone(), ProbeState::Supported);
        Some(ProbedRelay(relay.clone()))
    }

    /// An inbound `NEG-ERR` for `wire_sub_id` on `relay`: if this was a
    /// pending probe, classify `Unsupported` (cached — never re-probed on
    /// this connection). Returns `true` iff it was.
    pub fn on_neg_unsupported(&mut self, relay: &RelayUrl, wire_sub_id: &str) -> bool {
        if self
            .pending
            .remove(&(relay.clone(), wire_sub_id.to_string()))
            .is_some()
        {
            self.states.insert(relay.clone(), ProbeState::Unsupported);
            true
        } else {
            false
        }
    }
}

/// This module's own error type: either a malformed hex payload (an
/// untrusted-network fact, not a `::negentropy` protocol error) or the
/// wrapped `::negentropy::Error` itself.
#[derive(Debug)]
pub enum NegError {
    InvalidHex,
    Protocol(::negentropy::Error),
}

impl From<::negentropy::Error> for NegError {
    fn from(e: ::negentropy::Error) -> Self {
        NegError::Protocol(e)
    }
}

/// Outcome of processing one inbound `NEG-MSG` payload for an open
/// reconciliation session.
pub enum NegStep {
    /// More rounds remain: send this hex payload back as the next `NEG-MSG`.
    Continue(String),
    /// Reconciliation is complete (the client has nothing further to ask,
    /// per NIP-77 — there is no `NEG-DONE` wire frame; the client detects
    /// this locally): every event id the local store is missing, collected
    /// across every round of this session.
    Done(BTreeSet<EventId>),
}

/// A live, client-initiated negentropy reconciliation session (harvest
/// `nmp-nip77::runtime`'s `Reconciler`, re-cut). Wraps the external
/// `::negentropy::Negentropy` set-reconciler; `EngineCore` drives it
/// turn-by-turn from `on_relay_frame`'s `NEG-MSG` arm, exactly the way it
/// already drives REQ/EOSE.
pub struct Reconciler {
    negentropy: Negentropy<'static, NegentropyStorageVector>,
    have_ids: Vec<NegId>,
    need_ids: Vec<NegId>,
}

impl Reconciler {
    /// Open a new session seeded with `local_ids` — this side's current
    /// holdings for the filter being reconciled (`(created_at, EventId)`
    /// pairs, exactly what `EngineCore` reads back from its own store via
    /// `EventStore::query`). Returns the session plus the hex
    /// `initial_message` to place in the outbound `NEG-OPEN`.
    pub fn open(local_ids: &[(u64, EventId)]) -> (Self, String) {
        let mut storage = NegentropyStorageVector::with_capacity(local_ids.len());
        for (created_at, id) in local_ids {
            storage
                .insert(*created_at, NegId::from_byte_array(*id.as_bytes()))
                .expect("NegentropyStorageVector::insert never fails before seal");
        }
        storage
            .seal()
            .expect("a freshly-built, never-before-sealed storage always seals cleanly");

        let mut negentropy =
            Negentropy::owned(storage, 0).expect("frame_size_limit=0 (unlimited) is always valid");
        let initial = negentropy
            .initiate()
            .expect("a fresh Negentropy has never built an initial message before this call");

        (
            Self {
                negentropy,
                have_ids: Vec::new(),
                need_ids: Vec::new(),
            },
            hex::encode(initial),
        )
    }

    /// Feed one inbound `NEG-MSG` hex payload; drive exactly one reconcile
    /// round (harvest: `reconcile_with_ids`, the client/initiator method).
    /// `have_ids`/`need_ids` accumulate across every call on this session —
    /// the FULL set of ids this reconciliation has EVER decided we are
    /// missing is only complete once [`NegStep::Done`] is returned.
    pub fn step(&mut self, message_hex: &str) -> Result<NegStep, NegError> {
        let bytes = hex::decode(message_hex).map_err(|_| NegError::InvalidHex)?;
        let next =
            self.negentropy
                .reconcile_with_ids(&bytes, &mut self.have_ids, &mut self.need_ids)?;
        Ok(match next {
            Some(query) => NegStep::Continue(hex::encode(query)),
            None => NegStep::Done(
                self.need_ids
                    .iter()
                    .map(|id| {
                        EventId::from_slice(id.as_bytes())
                            .expect("a negentropy Id is always 32 bytes, same as EventId")
                    })
                    .collect(),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::negentropy::{Negentropy as RawNegentropy, NegentropyStorageVector as RawStorage};

    fn eid(byte: u8) -> EventId {
        EventId::from_byte_array([byte; 32])
    }

    fn relay(s: &str) -> RelayUrl {
        RelayUrl::parse(s).unwrap()
    }

    // ---- Prober FSM ------------------------------------------------------

    #[test]
    fn prober_transitions_unknown_to_probing_and_caches_the_supported_verdict() {
        let mut prober = Prober::new();
        let r = relay("wss://relay.example.com");

        assert_eq!(prober.state(&r), ProbeState::Unknown);
        assert!(prober.probed(&r).is_none());

        let req = prober
            .begin_probe(&r)
            .expect("Unknown must yield a probe request");
        assert_eq!(prober.state(&r), ProbeState::Probing);
        assert!(
            prober.probed(&r).is_none(),
            "Probing is not yet a capability token"
        );

        // Idempotent: re-probing while already Probing is a no-op.
        assert!(prober.begin_probe(&r).is_none());

        let wire = crate::core::wire_sub_id_string(&req.sub_id);
        let token = prober
            .on_neg_msg(&r, &wire)
            .expect("a NEG-MSG response classifies Supported");
        assert_eq!(token.url(), &r);
        assert_eq!(prober.state(&r), ProbeState::Supported);
        assert!(prober.probed(&r).is_some());

        // Cached: never re-probed once resolved.
        assert!(prober.begin_probe(&r).is_none());
    }

    #[test]
    fn prober_classifies_unsupported_on_neg_err_and_never_reprobes() {
        let mut prober = Prober::new();
        let r = relay("wss://relay.example.com");
        let req = prober.begin_probe(&r).unwrap();
        let wire = crate::core::wire_sub_id_string(&req.sub_id);

        assert!(prober.on_neg_unsupported(&r, &wire));
        assert_eq!(prober.state(&r), ProbeState::Unsupported);
        assert!(prober.probed(&r).is_none());
        assert!(
            prober.begin_probe(&r).is_none(),
            "an Unsupported verdict is cached, never re-probed"
        );
    }

    #[test]
    fn unrelated_wire_id_is_ignored_by_the_prober() {
        let mut prober = Prober::new();
        let r = relay("wss://relay.example.com");
        let _ = prober.begin_probe(&r);
        assert!(prober.on_neg_msg(&r, "not-a-real-sub-id").is_none());
        assert!(!prober.on_neg_unsupported(&r, "not-a-real-sub-id"));
        // The real pending probe is untouched by the unrelated lookup.
        assert_eq!(prober.state(&r), ProbeState::Probing);
    }

    // ---- Reconciler -------------------------------------------------------

    /// Drives our `Reconciler` (client/initiator) against a raw
    /// `::negentropy::Negentropy` (non-initiator) standing in for the
    /// relay side — the same round-trip shape as the `negentropy` crate's
    /// own `test_reconciliation_set`, but through THIS module's wrapper,
    /// proving `need_ids` surfaces exactly what the peer has and we don't.
    #[test]
    fn reconciler_discovers_exactly_the_ids_the_peer_has_and_we_do_not() {
        let shared = eid(0xaa);
        let client_only = eid(0xbb); // ours alone -- must never appear in `need_ids`.
        let relay_only_1 = eid(0x11);
        let relay_only_2 = eid(0x22);

        let local_ids = vec![(0u64, shared), (1u64, client_only)];
        let (mut reconciler, initial_hex) = Reconciler::open(&local_ids);

        let mut relay_storage = RawStorage::new();
        relay_storage
            .insert(0, NegId::from_byte_array(*shared.as_bytes()))
            .unwrap();
        relay_storage
            .insert(2, NegId::from_byte_array(*relay_only_1.as_bytes()))
            .unwrap();
        relay_storage
            .insert(3, NegId::from_byte_array(*relay_only_2.as_bytes()))
            .unwrap();
        relay_storage.seal().unwrap();
        let mut relay_side = RawNegentropy::owned(relay_storage, 0).unwrap();

        let mut hex_in = initial_hex;
        let mut rounds = 0;
        let need = loop {
            rounds += 1;
            assert!(
                rounds <= 10,
                "a 4-item reconciliation must converge in a handful of rounds -- bounded test"
            );
            let raw = hex::decode(&hex_in).expect("our own hex encoding must round-trip");
            let relay_reply = relay_side
                .reconcile(&raw)
                .expect("the raw relay-side reconcile must accept our message");
            match reconciler
                .step(&hex::encode(relay_reply))
                .expect("a well-formed relay reply must step cleanly")
            {
                NegStep::Continue(next) => hex_in = next,
                NegStep::Done(need_ids) => break need_ids,
            }
        };

        assert_eq!(
            need,
            BTreeSet::from([relay_only_1, relay_only_2]),
            "must need exactly the ids the peer has and we do not -- never our own extra id"
        );
    }
}
