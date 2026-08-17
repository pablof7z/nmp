//! Relay-session identity and NIP-42 AUTH readiness (#1606 step 3,
//! experimental fact-contract upgrade).
//!
//! EXPERIMENTAL / IN PROGRESS. This module is the result of an in-flight
//! attempt to answer a specific question: does upgrading this owner to
//! return named transition facts (rather than mutating cross-plane state
//! directly) actually separate "what changed about this session" from "what
//! else reacts to that" -- or does the entanglement documented in
//! `proposal-session-auth.md` make that separation impossible without also
//! moving cross-owner orchestration that does not belong here. See that
//! document for the full audit this module is built against.
//!
//! **What owns state here:** which physical connection currently speaks for
//! each relay session, and whether that connection has proven its identity
//! via NIP-42. Ten fields, all private -- module privacy is the enforcement
//! mechanism, per the standing rule against new crates for this purpose.
//!
//! **What does NOT own state here, on purpose:** the AUTH phase decision
//! (`apply_policy_completion` below) is the ONE transition converted to the
//! fact-contract shape so far. Every other session/AUTH transition
//! (`on_relay_connected`, `on_relay_disconnected`, `on_auth_challenge`,
//! `invalidate_auth_epoch`, `park_relay_lanes_for_auth`,
//! `deny_write_lanes_for_auth`) stays in `auth_transport.rs`, as `impl
//! EngineCore` methods that call into this owner's narrow methods rather
//! than touching its fields. That is not an oversight -- see
//! `auth_transport.rs`'s module doc for why each of those resisted the same
//! conversion.

use super::*;

/// Ten fields; every one already had a doc comment naming this
/// responsibility before this module existed (see `core/mod.rs`'s original
/// declarations, moved here verbatim with their comments).
///
/// **Visibility note, honestly stated rather than laundered:** these are
/// `pub(super)` -- visible to all of `core` -- not fully private. The three
/// named reach-ins this task targeted (`write.rs:1296`, `write.rs:5812-5814`,
/// `on_auth_policy_completed`'s dance) are converted to the methods below and
/// no longer touch these fields directly. What is NOT converted, found while
/// doing this and left `pub(super)` rather than silently reverted: `mod.rs`'s
/// `diagnostics_snapshot` and its `EngineMsg::RelayOpenFailed` arm, and
/// `auth_transport.rs`'s `on_relay_connected`/`on_relay_disconnected`/
/// `invalidate_auth_epoch`/`park_relay_lanes_for_auth` still read and mutate
/// these fields raw. Making the fields fully private would have meant either
/// writing ~15 more narrow accessor methods sight-unseen or moving those five
/// functions bodily into this file -- both real options, neither attempted
/// here; see `proposal-session-auth.md` and this task's final report for why.
#[derive(Default)]
pub(super) struct SessionRegistry {
    /// EngineCore's memory of the exact connection generation and SESSION
    /// that currently occupy each pool slot. Disconnects are asynchronous;
    /// the generation prevents a delayed old disconnect from erasing a slot
    /// that has already reopened, and the session key prevents a frame
    /// reported for one access context from ever being read as another's
    /// (#8: both halves of the (handle, session) pair must match exactly).
    pub(super) slot_to_relay: HashMap<u32, (TransportRelayHandle, RelaySessionKey)>,
    /// Sessions CURRENTLY connected -- feeds `AcquisitionEvidence.sources[_]
    /// .status` (`Requesting` iff a member here covers the atom;
    /// `Disconnected` iff it was a member of `ever_connected_relays` but
    /// isn't a member here; `Connecting` otherwise). Additive bookkeeping:
    /// `slot_to_relay`'s own semantics (populated on connect, never cleared
    /// on disconnect) are untouched by this.
    pub(super) connected_relays: BTreeSet<RelaySessionKey>,
    /// Every session that has connected at least once, ever -- distinguishes
    /// `Disconnected` (was connected, dropped) from `Connecting` (never yet
    /// connected) for the same evidence computation.
    pub(super) ever_connected_relays: BTreeSet<RelaySessionKey>,
    /// The exact connection generation that has completed NIP-42 AUTH for
    /// each PROTECTED session (#8). Public sessions never enter this map. A
    /// fresh generation is never pre-authorized (`on_relay_connected` removes
    /// the entry), and readiness dies with the connection
    /// (`on_relay_disconnected` removes it too) -- so "ready" always means
    /// "THIS socket, after THIS socket's AUTH handshake", never an earlier
    /// generation's leftover.
    pub(super) auth_ready_sessions: HashMap<RelaySessionKey, TransportRelayHandle>,
    /// Newly connected author sessions whose first inbound frame is still
    /// being observed for a proactive AUTH challenge. Unlike sticky
    /// `auth_required_sessions`, this exact-generation gate is released by a
    /// transport's ordered first-read completion when an ordinary relay has
    /// no already-available challenge.
    pub(super) auth_probe_sessions: HashMap<RelaySessionKey, TransportRelayHandle>,
    /// Exact live sessions for which the relay has actually required AUTH:
    /// an AUTH challenge, auth-required write response, or restricted close.
    /// Merely using a frozen NIP-42 access identity does not populate this
    /// set; ordinary relays are released only after the transport's ordered
    /// first socket read-drain completes without an available challenge.
    pub(super) auth_required_sessions: BTreeSet<RelaySessionKey>,
    /// Current reducer-owned AUTH epoch for each exact protected session.
    /// Entries are removed on disconnect/reconnect teardown; the separate
    /// monotonic counters below deliberately survive that removal so stale
    /// callbacks can never alias a future generation.
    pub(super) auth_sessions: HashMap<RelaySessionKey, AuthSessionState>,
    pub(super) next_auth_epoch: Option<u64>,
    pub(super) next_auth_operation: Option<u64>,
    /// Runtime relay-worker open failures keyed by their exact current owner.
    /// Entries are pruned whenever demand/write ownership changes and cleared
    /// by a successful connection for that session.
    pub(super) relay_open_failures: BTreeMap<RelaySessionKey, String>,
}

/// The write plane's answer to "may a lane on this session attempt now."
/// Replaces the three-map read open-coded at the write plane's call site
/// (`schedule_ready`) before this conversion.
pub(super) enum WriteGate {
    Open,
    AwaitingProbe,
    AwaitingAuth,
}

/// What `apply_policy_completion` decided, for the coordinator to route.
/// Four variants -- well inside the ~8 the task's failure criteria named,
/// though see this module's doc and `auth_transport.rs`'s for why that
/// count staying small didn't turn out to be the hard part.
pub(super) enum AuthTransition {
    NoChange,
    SignatureRequested {
        token: AuthOpToken,
        unsigned: UnsignedEvent,
    },
    Denied {
        session: RelaySessionKey,
        source: StoredAuthDenialSource,
        reason: String,
    },
    Errored {
        // Carried but not yet consumed anywhere: `on_auth_policy_completed`'s
        // routing currently does nothing with an `Errored` transition beyond
        // the shared evidence refresh every arm gets. This is precisely the
        // observability gap the design this module tests for was supposed to
        // close -- a fail-open-shaped guard (silently dropping AUTH state)
        // now produces a named, inspectable value instead of nothing, but
        // nothing downstream reads it yet. Wiring it to a diagnostics-visible
        // fact is the natural next step and is deliberately NOT done here:
        // it is new behavior beyond the three conversions this task scoped.
        #[allow(dead_code)]
        session: RelaySessionKey,
    },
}

impl SessionRegistry {
    pub(super) fn new() -> Self {
        Self {
            next_auth_epoch: Some(1),
            next_auth_operation: Some(1),
            ..Self::default()
        }
    }

    // ---- minting -----------------------------------------------------

    /// `u64::MAX` is structurally reserved for [`AUTH_SEQUENCE_SENTINEL`]:
    /// the counter treats it as already-exhausted and never issues it, so a
    /// REAL epoch/operation sequence can never compare equal to the
    /// counter-exhausted fallback epoch `on_auth_challenge`/
    /// `on_auth_restricted` install. Sentinel distinctness therefore no
    /// longer rests on the `Error`-phase guard alone (#8 U2's deferred
    /// latent item): even a registry or correlation path that only compares
    /// epochs is safe.
    ///
    /// Deliberately a free function over `&mut Option<u64>`, not a `&mut
    /// self` method: `apply_policy_completion` needs to mint a sequence
    /// while a `&mut AuthSessionState` borrowed from `self.auth_sessions` is
    /// still live, and a `&mut self` method here would collapse that
    /// disjoint-field borrow back into "all of self." This shape existed
    /// before this module did; it turned out to be exactly the escape hatch
    /// the conversion needed.
    pub(super) fn mint_auth_sequence(next: &mut Option<u64>) -> Option<u64> {
        let issued = (*next)?;
        if issued == AUTH_SEQUENCE_SENTINEL {
            *next = None;
            return None;
        }
        *next = issued.checked_add(1);
        Some(issued)
    }

    pub(super) fn mint_auth_epoch(
        &mut self,
        handle: TransportRelayHandle,
        session: &RelaySessionKey,
    ) -> Option<AuthEpoch> {
        Some(AuthEpoch {
            handle,
            session: session.clone(),
            sequence: Self::mint_auth_sequence(&mut self.next_auth_epoch)?,
        })
    }

    pub(super) fn mint_auth_operation(&mut self, epoch: &AuthEpoch) -> Option<AuthOpToken> {
        Some(AuthOpToken {
            epoch: epoch.clone(),
            sequence: Self::mint_auth_sequence(&mut self.next_auth_operation)?,
        })
    }

    // ---- current facts (I3, and the write-plane's named questions) ---

    pub(super) fn exact_current_auth_epoch(&self, epoch: &AuthEpoch) -> bool {
        self.connected_relays.contains(&epoch.session)
            && matches!(
                self.slot_to_relay.get(&epoch.handle.slot),
                Some((handle, session)) if *handle == epoch.handle && *session == epoch.session
            )
            && self
                .auth_sessions
                .get(&epoch.session)
                .is_some_and(|state| state.epoch == *epoch)
    }

    pub(super) fn is_current_transport_session(
        &self,
        handle: TransportRelayHandle,
        session: &RelaySessionKey,
    ) -> bool {
        self.connected_relays.contains(session)
            && matches!(
                self.slot_to_relay.get(&handle.slot),
                Some((current, current_session))
                    if *current == handle && current_session == session
            )
    }

    pub(super) fn is_connected(&self, session: &RelaySessionKey) -> bool {
        self.connected_relays.contains(session)
    }

    /// Replaces `write.rs`'s pre-conversion three-map read
    /// (`auth_probe_sessions` present, OR `auth_required_sessions` present
    /// without `auth_ready_sessions`) at the gate `schedule_ready` uses
    /// before allocating an attempt ordinal.
    pub(super) fn write_gate(&self, session: &RelaySessionKey) -> WriteGate {
        if self.auth_probe_sessions.contains_key(session) {
            WriteGate::AwaitingProbe
        } else if self.auth_required_sessions.contains(session)
            && !self.auth_ready_sessions.contains_key(session)
        {
            WriteGate::AwaitingAuth
        } else {
            WriteGate::Open
        }
    }

    /// Whether the relay actually REQUIRED auth for this session (challenge,
    /// auth-required write ack, or restricted close). `invalidate_auth_epoch`
    /// asks this to decide whether there is anything to park.
    pub(super) fn requires_auth(&self, session: &RelaySessionKey) -> bool {
        self.auth_required_sessions.contains(session)
    }

    // ---- narrow removal doors, deliberately NOT one wide transition ----
    //
    // `invalidate_auth_epoch` (auth_transport.rs) interleaves these two
    // removals with a cross-plane call
    // (`abandon_session_subs`, demand execution) and a conditional effect
    // (`close_protected_reqs`) that depends on the FIRST removal's result
    // and must run before the SECOND. That ordering is why this is two
    // narrow doors instead of one `AuthEpochInvalidated` fact: a single
    // transition method here could not run the coordinator's own call in
    // the middle of its own body without taking `&mut EngineCore`, which
    // would defeat the whole point of the field being private. Found while
    // attempting exactly that conversion; recorded rather than forced.

    /// Remove and report whether this session had completed AUTH. Called
    /// BEFORE `abandon_session_subs` in `invalidate_auth_epoch` -- the
    /// caller needs the pre-removal answer to decide whether to close any
    /// protected REQs already in flight.
    pub(super) fn take_ready(&mut self, session: &RelaySessionKey) -> bool {
        self.auth_ready_sessions.remove(session).is_some()
    }

    /// Remove and return this session's AUTH state, if any. Called AFTER
    /// `abandon_session_subs` in `invalidate_auth_epoch` -- the two removals
    /// are not adjacent in that function on purpose (see above).
    pub(super) fn take_auth_state(
        &mut self,
        session: &RelaySessionKey,
    ) -> Option<AuthSessionState> {
        self.auth_sessions.remove(session)
    }

    // ---- `on_relay_connected`'s narrow doors ---------------------------
    //
    // Unlike `invalidate_auth_epoch`, most of `on_relay_connected`'s session
    // mutations genuinely ARE adjacent and self-contained -- the exception
    // is the displaced-session teardown, which still calls
    // `invalidate_auth_epoch` (a coordinator-level cross-owner call) in the
    // middle of its own three-line sequence, for the identical reason as
    // above. The rest below separates cleanly. Measured by actually writing
    // both kinds and finding out which compiled into something smaller vs.
    // which just relocated the same three lines behind a name.

    /// Whether this handle already owns this physical slot with this exact
    /// session -- narrower than `is_current_transport_session`, which also
    /// requires `connected_relays` to already contain the session. At the
    /// point `on_relay_connected` asks this, the session is not yet
    /// connected (that is what this call is deciding), so the wider
    /// predicate would give the wrong answer.
    pub(super) fn same_slot_owner(
        &self,
        handle: TransportRelayHandle,
        session: &RelaySessionKey,
    ) -> bool {
        matches!(
            self.slot_to_relay.get(&handle.slot),
            Some((current, current_session))
                if *current == handle && current_session == session
        )
    }

    /// The generation currently occupying `handle`'s slot is newer than
    /// `handle` itself -- a delayed old `RelayConnected` for a superseded
    /// generation.
    pub(super) fn is_stale_generation(&self, handle: TransportRelayHandle) -> bool {
        self.slot_to_relay
            .get(&handle.slot)
            .is_some_and(|(current, _)| current.generation > handle.generation)
    }

    /// The session currently occupying this slot, if any -- read BEFORE
    /// `claim_slot` overwrites it. Returns `None` when nothing does, or when
    /// the occupant IS the incoming session (nothing displaced).
    pub(super) fn displaced_slot_owner(
        &self,
        handle: TransportRelayHandle,
        incoming: &RelaySessionKey,
    ) -> Option<RelaySessionKey> {
        let (_, occupant) = self.slot_to_relay.get(&handle.slot)?;
        (occupant != incoming).then(|| occupant.clone())
    }

    /// The two raw removals `on_relay_connected` performs for a displaced
    /// session, AFTER it has already called `invalidate_auth_epoch` for that
    /// session -- deliberately not folded into `invalidate_auth_epoch`
    /// itself, which handles the CURRENT session's own epoch, not a
    /// slot's previous occupant's connectivity/probe bookkeeping.
    pub(super) fn release_displaced(&mut self, displaced: &RelaySessionKey) {
        self.connected_relays.remove(displaced);
        self.auth_probe_sessions.remove(displaced);
    }

    pub(super) fn clear_open_failure(&mut self, session: &RelaySessionKey) -> bool {
        self.relay_open_failures.remove(session).is_some()
    }

    /// `on_relay_disconnected`'s entry guard: the exact (handle, session)
    /// pair must still occupy the slot, or this is a delayed old disconnect
    /// for a superseded generation (or a session that no longer occupies
    /// this slot) and must not tear down whatever is actually live there
    /// now. Returns the validated session, owned, for the rest of the
    /// (long) transition to use -- it does not remove anything itself.
    pub(super) fn validated_occupant(
        &self,
        handle: TransportRelayHandle,
        reported_session: &RelaySessionKey,
    ) -> Option<RelaySessionKey> {
        let (current, session) = self.slot_to_relay.get(&handle.slot)?;
        (*current == handle && session == reported_session).then(|| session.clone())
    }

    /// `on_relay_disconnected`'s own two removals, kept separate from
    /// `release_displaced` even though the field operations are identical --
    /// same shape, different caller and different reason, and folding them
    /// into one name would blur which transition is being reported.
    /// `ever_connected_relays` is deliberately untouched: a subsequent
    /// evidence computation must read `Disconnected`, never `Connecting`.
    pub(super) fn mark_disconnected(&mut self, session: &RelaySessionKey) {
        self.connected_relays.remove(session);
        self.auth_probe_sessions.remove(session);
    }

    /// Claim the slot for `(handle, session)`, mark the session connected
    /// (both the current and the append-only ever-connected sets), and
    /// resolve probe bookkeeping for a genuinely NEW physical connection.
    /// `same_physical_session` decides the last part: an already-live handle
    /// re-reporting `RelayConnected` (not a reconnect) must not re-arm or
    /// clear probe state that a prior call on this exact handle already
    /// settled.
    pub(super) fn claim_slot(
        &mut self,
        handle: TransportRelayHandle,
        session: &RelaySessionKey,
        same_physical_session: bool,
    ) {
        self.slot_to_relay
            .insert(handle.slot, (handle, session.clone()));
        self.connected_relays.insert(session.clone());
        self.ever_connected_relays.insert(session.clone());
        if !same_physical_session && session.access != AccessContext::Public {
            if self.auth_required_sessions.contains(session) {
                self.auth_probe_sessions.remove(session);
            } else {
                self.auth_probe_sessions.insert(session.clone(), handle);
            }
        }
    }

    /// The relay's ordinary public read session, unless THIS session has
    /// already completed AUTH -- replaces the ready-or-public choice
    /// `schedule_ready` open-coded before asking the coordinate gate.
    pub(super) fn authenticated_view(&self, session: &RelaySessionKey) -> RelaySessionKey {
        if self.auth_ready_sessions.contains_key(session) {
            session.clone()
        } else {
            RelaySessionKey::public(session.relay.clone())
        }
    }

    // ---- transitions with exactly one consumer (direct calls, no enum) -

    /// `NotHandedOff` is the transport reporting it has NO session for this
    /// relay -- `pool.ensure_session` failed, so nothing was sent and no
    /// socket was observed closing. That is a connectivity fact, so the
    /// session leaves `connected_relays`.
    ///
    /// It deliberately does NOT touch `slot_to_relay` or `auth_sessions`,
    /// and the resulting two-thirds state is safe in exactly one direction.
    /// Every predicate over those three (`exact_current_auth_epoch`,
    /// `is_current_transport_session`) is a CONJUNCTION, so a missing term
    /// can only cause a rejection, never an acceptance: the cost is a
    /// discarded AUTH operation, never an accepted stale one.
    ///
    /// The state repairs itself through the caller's `EnsureWriteRelay`. A
    /// protected reconnect runs `invalidate_auth_epoch` BEFORE re-inserting
    /// into `connected_relays`, so the auth entries left behind here are
    /// cleared by the same edge that restores connectivity -- in that order,
    /// which is what makes it safe.
    ///
    /// Do not "complete" this by also clearing `slot_to_relay` or
    /// `auth_sessions`: this path never observed a generation end, so
    /// retiring one here would discard a socket that is still live for
    /// reads.
    pub(super) fn transport_has_no_session(&mut self, session: &RelaySessionKey) {
        self.connected_relays.remove(session);
    }

    /// The write plane observed a relay respond `OK false auth-required:` --
    /// the relay revealed AUTH requirement through a write ack rather than a
    /// proactive challenge or restricted close.
    pub(super) fn relay_demanded_auth(&mut self, session: &RelaySessionKey) {
        self.auth_probe_sessions.remove(session);
        self.auth_required_sessions.insert(session.clone());
    }

    // ---- the AUTH phase decision --------------------------------------

    /// Apply one policy completion to the session's AUTH phase and report
    /// what changed, without the state ever leaving `auth_sessions`.
    ///
    /// The predecessor of this method (`auth_transport.rs`'s
    /// `on_auth_policy_completed`) removed the session's `AuthSessionState`
    /// from the map, matched the phase, and reinserted it at eight different
    /// exits -- and ONE exit (the `Allow` branch's `AccessContext::Nip42`
    /// destructure, unreachable today as far as I've verified but not
    /// proven so from every mint site) returned WITHOUT reinserting,
    /// silently dropping the session's AUTH state. That branch is now
    /// `AuthTransition::Errored` instead of a silent no-op — the fix this
    /// conversion was supposed to produce, and did.
    pub(super) fn apply_policy_completion(
        &mut self,
        token: AuthOpToken,
        instance: Option<AuthCapabilityInstance>,
        outcome: AuthPolicyOutcome,
        now: Timestamp,
    ) -> AuthTransition {
        if !self.exact_current_auth_epoch(&token.epoch) {
            return AuthTransition::NoChange;
        }
        let session = token.epoch.session.clone();
        let Some(state) = self.auth_sessions.get_mut(&session) else {
            return AuthTransition::NoChange;
        };
        if !matches!(
            &state.phase,
            AuthSessionPhase::AwaitingPolicy { token: current } if *current == token
        ) {
            return AuthTransition::NoChange;
        }
        let missing_capability = instance.is_none()
            && state.policy_instance.is_none()
            && matches!(outcome, AuthPolicyOutcome::Unavailable);
        let exact_bound = instance.is_some() && instance == state.policy_instance;
        if !missing_capability && !exact_bound {
            return AuthTransition::NoChange;
        }

        match outcome {
            AuthPolicyOutcome::Allow => {
                let AccessContext::Nip42(expected_pubkey) = state.epoch.session.access else {
                    state.phase = AuthSessionPhase::Error;
                    return AuthTransition::Errored { session };
                };
                let clock = now.as_secs();
                let minimum = match state.last_created_at {
                    Some(last) => {
                        let Some(next) = last.as_secs().checked_add(1) else {
                            state.phase = AuthSessionPhase::Error;
                            return AuthTransition::Errored { session };
                        };
                        next.max(clock)
                    }
                    None => clock,
                };
                let Some(maximum) = clock.checked_add(AUTH_MAX_FUTURE_SECS) else {
                    state.phase = AuthSessionPhase::Error;
                    return AuthTransition::Errored { session };
                };
                if minimum > maximum {
                    state.phase = AuthSessionPhase::Error;
                    return AuthTransition::Errored { session };
                }
                let created_at = Timestamp::from(minimum);
                let unsigned = NostrEventBuilder::auth(
                    state.challenge.clone(),
                    state.epoch.session.relay.clone(),
                )
                .custom_created_at(created_at)
                .build(expected_pubkey);
                let epoch = state.epoch.clone();
                // Disjoint-field mint: `state` still borrows
                // `self.auth_sessions`; this borrows `self.next_auth_operation`
                // directly, never through a `&mut self` method. See this
                // type's `mint_auth_sequence` doc.
                let Some(sequence) = Self::mint_auth_sequence(&mut self.next_auth_operation) else {
                    let Some(state) = self.auth_sessions.get_mut(&session) else {
                        return AuthTransition::Errored { session };
                    };
                    state.phase = AuthSessionPhase::Error;
                    return AuthTransition::Errored { session };
                };
                let sign_token = AuthOpToken { epoch, sequence };
                let Some(state) = self.auth_sessions.get_mut(&session) else {
                    // Exhaustion above already returned; reaching here means
                    // nothing removed the entry between the two `get_mut`
                    // calls (nothing in this function does), so this arm is
                    // unreachable in practice. Typed rather than `expect`ed,
                    // matching this function's whole point: no branch here
                    // silently drops state.
                    return AuthTransition::Errored { session };
                };
                state.last_created_at = Some(created_at);
                state.policy_instance = instance;
                state.phase = AuthSessionPhase::AwaitingSignature {
                    token: sign_token.clone(),
                    unsigned: unsigned.clone(),
                };
                AuthTransition::SignatureRequested {
                    token: sign_token,
                    unsigned,
                }
            }
            AuthPolicyOutcome::Deny { reason } => {
                state.phase = AuthSessionPhase::Denied;
                AuthTransition::Denied {
                    session,
                    source: StoredAuthDenialSource::Policy,
                    reason,
                }
            }
            AuthPolicyOutcome::Unavailable | AuthPolicyOutcome::Error { .. } => {
                state.phase = AuthSessionPhase::Error;
                AuthTransition::Errored { session }
            }
        }
    }
}
