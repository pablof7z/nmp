//! NIP-42 authentication and relay-session transport orchestration.
//!
//! This module owns challenge epochs, capability binding, connection-generation
//! checks, inbound frame reduction, and the transition between authenticated
//! sessions and ordinary query/write work.

use nmp_grammar::RelaySessionKey;
use super::*;

impl CoreState {
    // ---- transport wiring (slot bookkeeping only — C owns the pool) -----

    /// `u64::MAX` is structurally reserved for [`AUTH_SEQUENCE_SENTINEL`]:
    /// the counter treats it as already-exhausted and never issues it, so a
    /// REAL epoch/operation sequence can never compare equal to the
    /// counter-exhausted fallback epoch `on_auth_challenge`/
    /// `on_auth_restricted` install. Sentinel distinctness therefore no
    /// longer rests on the `Error`-phase guard alone (#8 U2's deferred
    /// latent item): even a registry or correlation path that only compares
    /// epochs is safe.
    pub(in crate::core) fn mint_auth_sequence(next: &mut Option<u64>) -> Option<u64> {
        let issued = (*next)?;
        if issued == AUTH_SEQUENCE_SENTINEL {
            *next = None;
            return None;
        }
        *next = issued.checked_add(1);
        Some(issued)
    }

    pub(in crate::core) fn mint_auth_epoch(
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

    pub(in crate::core) fn mint_auth_operation(
        &mut self,
        epoch: &AuthEpoch,
    ) -> Option<AuthOpToken> {
        Some(AuthOpToken {
            epoch: epoch.clone(),
            sequence: Self::mint_auth_sequence(&mut self.next_auth_operation)?,
        })
    }

    pub(in crate::core) fn exact_current_auth_epoch(&self, epoch: &AuthEpoch) -> bool {
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

    pub(in crate::core) fn is_current_transport_session(
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

    pub(in crate::core) fn close_protected_reqs(
        &self,
        session: &RelaySessionKey,
    ) -> Option<Effect> {
        let ops: Vec<_> = self
            .router
            .plan()
            .reqs
            .get(session)?
            .iter()
            .map(|req| WireOp::Close(req.sub_id.clone()))
            .collect();
        (!ops.is_empty()).then(|| {
            Effect::Wire(self.attempted_wire_delta(WireDelta {
                ops: vec![(session.clone(), ops)],
            }))
        })
    }

    pub(in crate::core) fn park_relay_lanes_for_auth(
        &mut self,
        session: &RelaySessionKey,
        effects: &mut Vec<Effect>,
    ) {
        let Ok(lanes) = self.recover_all_lanes() else {
            return;
        };
        for (id, lane) in lanes {
            let Some(pending) = self.pending.get(&id) else {
                continue;
            };
            let lane_session = RelaySessionKey::new(
                lane.key.relay.clone(),
                Some(pending.signing_pubkey),
            );
            if &lane_session != session {
                continue;
            }
            let transitioned = match lane.state {
                PublishQueueLaneState::WaitingAuth | PublishQueueLaneState::Terminal { .. } => {
                    continue
                }
                PublishQueueLaneState::InFlight { ordinal, .. } => {
                    let committed = self.commit_lane_suspension(
                        &lane.key,
                        lane.revision,
                        ordinal,
                        self.clock,
                        PublishQueueTransientCause::AuthRequired,
                        Some("AUTH challenge received".to_string()),
                        true,
                    );
                    if committed.is_ok() {
                        self.remove_active_lane(id, &lane.key.relay);
                    }
                    committed
                }
                PublishQueueLaneState::WaitingConnection
                | PublishQueueLaneState::Eligible { .. }
                | PublishQueueLaneState::Transient { .. } => {
                    self.commit_lane_waiting(&lane.key, lane.revision, true)
                }
            };
            if transitioned.is_err() {
                continue;
            }
            self.emit_write_fact(
                id,
                WriteFact::Relay {
                    event_id: lane.key.event_id,
                    relay: session.relay.clone(),
                    state: RelayState::Waiting(RelayWaiting::NeedsAuth),
                },
                effects,
            );
        }
    }

    pub(in crate::core) fn invalidate_auth_epoch(
        &mut self,
        session: &RelaySessionKey,
        close_wire: bool,
        effects: &mut Vec<Effect>,
    ) -> Option<AuthSessionState> {
        let was_ready = self.auth_ready_sessions.remove(session).is_some();
        self.abandon_session_subs(session);
        if close_wire && was_ready {
            if let Some(close) = self.close_protected_reqs(session) {
                effects.push(close);
            }
        }
        let previous = self.auth_sessions.remove(session);
        if let Some(state) = previous.as_ref() {
            effects.push(Effect::RelayAuth(AuthEffect::Cancel(state.epoch.clone())));
        }
        // Park only when the relay actually REQUIRED auth for this session
        // (`auth_required_sessions`: challenge, auth-required write ack, or
        // restricted close — every path that creates reducer-known AUTH
        // truth inserts it first). A session that was never required has
        // nothing to invalidate, and its writes deliberately proceed on the
        // ordinary connectivity path (`schedule_ready`'s gate mirrors this):
        // parking such lanes as `WaitingAuth` would wedge every write to a
        // relay that never challenges, because the ONLY wake for
        // `WaitingAuth` is `finish_auth_ok` — which for that relay never
        // fires.
        if self.auth_required_sessions.contains(session) {
            self.park_relay_lanes_for_auth(session, effects);
        }
        previous
    }

    pub(in crate::core) fn on_auth_challenge(
        &mut self,
        handle: TransportRelayHandle,
        session: RelaySessionKey,
        challenge: String,
    ) -> Vec<Effect> {
        let Some(expected_pubkey) = session.authenticate_as else {
            return Vec::new();
        };
        self.auth_probe_sessions.remove(&session);
        self.auth_required_sessions.insert(session.clone());
        let mut effects = Vec::new();
        let previous = self.invalidate_auth_epoch(&session, true, &mut effects);
        let last_created_at = previous.as_ref().and_then(|state| state.last_created_at);
        let fallback_epoch = previous.map(|state| state.epoch);
        let Some(epoch) = self.mint_auth_epoch(handle, &session) else {
            self.auth_sessions.insert(
                session.clone(),
                AuthSessionState {
                    epoch: fallback_epoch.unwrap_or(AuthEpoch {
                        handle,
                        session,
                        sequence: AUTH_SEQUENCE_SENTINEL,
                    }),
                    challenge,
                    last_created_at,
                    policy_instance: None,
                    signer_instance: None,
                    phase: AuthSessionPhase::Error,
                },
            );
            self.refresh_all_observation_evidence(&mut effects);
            return effects;
        };
        if challenge.is_empty() {
            self.auth_sessions.insert(
                session,
                AuthSessionState {
                    epoch,
                    challenge,
                    last_created_at,
                    policy_instance: None,
                    signer_instance: None,
                    phase: AuthSessionPhase::Error,
                },
            );
            self.refresh_all_observation_evidence(&mut effects);
            return effects;
        }
        let Some(token) = self.mint_auth_operation(&epoch) else {
            self.auth_sessions.insert(
                session,
                AuthSessionState {
                    epoch,
                    challenge,
                    last_created_at,
                    policy_instance: None,
                    signer_instance: None,
                    phase: AuthSessionPhase::Error,
                },
            );
            self.refresh_all_observation_evidence(&mut effects);
            return effects;
        };
        self.auth_sessions.insert(
            session,
            AuthSessionState {
                epoch: epoch.clone(),
                challenge: challenge.clone(),
                last_created_at,
                policy_instance: None,
                signer_instance: None,
                phase: AuthSessionPhase::AwaitingPolicy {
                    token: token.clone(),
                },
            },
        );
        effects.push(Effect::RelayAuth(AuthEffect::RequestPolicy {
            token,
            expected_pubkey,
            challenge,
        }));
        self.refresh_all_observation_evidence(&mut effects);
        effects
    }

    pub(in crate::core) fn on_auth_restricted(
        &mut self,
        handle: TransportRelayHandle,
        session: RelaySessionKey,
    ) -> Vec<Effect> {
        if session.authenticate_as.is_none() {
            return Vec::new();
        }
        self.auth_probe_sessions.remove(&session);
        self.auth_required_sessions.insert(session.clone());
        let mut effects = Vec::new();
        let previous = self.invalidate_auth_epoch(&session, true, &mut effects);
        let last_created_at = previous.as_ref().and_then(|state| state.last_created_at);
        let fallback_epoch = previous.map(|state| state.epoch);
        let epoch = self
            .mint_auth_epoch(handle, &session)
            .or(fallback_epoch)
            .unwrap_or(AuthEpoch {
                handle,
                session: session.clone(),
                sequence: AUTH_SEQUENCE_SENTINEL,
            });
        self.auth_sessions.insert(
            session,
            AuthSessionState {
                epoch,
                challenge: String::new(),
                last_created_at,
                policy_instance: None,
                signer_instance: None,
                phase: AuthSessionPhase::Denied,
            },
        );
        self.refresh_all_observation_evidence(&mut effects);
        effects
    }

    /// Terminalize only lanes that are durably waiting on this exact
    /// authenticated session. Subscription `CLOSED` facts never call this
    /// door: only typed policy/signer denial and an exact-correlated AUTH
    /// `OK false` do.
    pub(in crate::core) fn deny_write_lanes_for_auth(
        &mut self,
        session: &RelaySessionKey,
        source: StoredAuthDenialSource,
        reason: String,
        effects: &mut Vec<Effect>,
    ) {
        let Some(pubkey) = session.authenticate_as else {
            return;
        };
        let lanes = match self.recover_all_lanes() {
            Ok(lanes) => lanes,
            Err(_) => {
                return;
            }
        };
        let public_source = match source {
            StoredAuthDenialSource::Policy => AuthDenialSource::Policy,
            StoredAuthDenialSource::Signer => AuthDenialSource::Signer,
            StoredAuthDenialSource::Relay => AuthDenialSource::Relay,
        };
        for (id, lane) in lanes {
            let exact_session = self
                .pending
                .get(&id)
                .map(|pending| {
                    RelaySessionKey::new(
                        lane.key.relay.clone(),
                        Some(pending.signing_pubkey),
                    )
                })
                .is_some_and(|candidate| candidate == *session);
            if !exact_session || !matches!(lane.state, PublishQueueLaneState::WaitingAuth) {
                continue;
            }
            let denial = StoredAuthDenial {
                source,
                reason: reason.clone(),
            };
            match self.commit_lane_auth_denied(&lane.key, lane.revision, denial) {
                Ok(_) => {
                    self.remove_active_lane(id, &lane.key.relay);
                    self.emit_write_fact(
                        id,
                        WriteFact::Relay {
                            event_id: lane.key.event_id,
                            relay: lane.key.relay.clone(),
                            state: RelayState::AuthFailed {
                                pubkey,
                                source: public_source,
                                reason: reason.clone(),
                            },
                        },
                        effects,
                    );
                    self.close_if_all_lanes_terminal(id, effects);
                }
                Err(_) => {
                    // Commit-before-emit: the receipt must never claim a
                    // terminal fact the store did not establish.
                }
            }
        }
        effects.extend(self.schedule_ready(self.clock));
    }

    pub(in crate::core) fn on_auth_policy_completed(
        &mut self,
        token: AuthOpToken,
        instance: Option<AuthCapabilityInstance>,
        outcome: AuthPolicyOutcome,
    ) -> Vec<Effect> {
        if !self.exact_current_auth_epoch(&token.epoch) {
            return Vec::new();
        }
        let session = token.epoch.session.clone();
        let Some(mut state) = self.auth_sessions.remove(&session) else {
            return Vec::new();
        };
        if !matches!(
            &state.phase,
            AuthSessionPhase::AwaitingPolicy { token: current } if *current == token
        ) {
            self.auth_sessions.insert(session, state);
            return Vec::new();
        }
        let missing_capability = instance.is_none()
            && state.policy_instance.is_none()
            && matches!(outcome, AuthPolicyOutcome::Unavailable);
        let exact_bound = instance.is_some() && instance == state.policy_instance;
        if !missing_capability && !exact_bound {
            self.auth_sessions.insert(session, state);
            return Vec::new();
        }
        let mut effects = Vec::new();
        let denial = match outcome {
            AuthPolicyOutcome::Allow => {
                let Some(expected_pubkey) = state.epoch.session.authenticate_as else {
                    return Vec::new();
                };
                let clock = self.clock.as_secs();
                let minimum = match state.last_created_at {
                    Some(last) => {
                        let Some(next) = last.as_secs().checked_add(1) else {
                            state.phase = AuthSessionPhase::Error;
                            self.auth_sessions.insert(session, state);
                            self.refresh_all_observation_evidence(&mut effects);
                            return effects;
                        };
                        next.max(clock)
                    }
                    None => clock,
                };
                let Some(maximum) = clock.checked_add(AUTH_MAX_FUTURE_SECS) else {
                    state.phase = AuthSessionPhase::Error;
                    self.auth_sessions.insert(session, state);
                    self.refresh_all_observation_evidence(&mut effects);
                    return effects;
                };
                if minimum > maximum {
                    state.phase = AuthSessionPhase::Error;
                    self.auth_sessions.insert(session, state);
                    self.refresh_all_observation_evidence(&mut effects);
                    return effects;
                }
                let created_at = Timestamp::from(minimum);
                let unsigned = NostrEventBuilder::auth(
                    state.challenge.clone(),
                    state.epoch.session.relay.clone(),
                )
                .custom_created_at(created_at)
                .build(expected_pubkey);
                let Some(sign_token) = self.mint_auth_operation(&state.epoch) else {
                    state.phase = AuthSessionPhase::Error;
                    self.auth_sessions.insert(session, state);
                    self.refresh_all_observation_evidence(&mut effects);
                    return effects;
                };
                state.last_created_at = Some(created_at);
                state.policy_instance = instance;
                state.phase = AuthSessionPhase::AwaitingSignature {
                    token: sign_token.clone(),
                    unsigned: unsigned.clone(),
                };
                effects.push(Effect::RelayAuth(AuthEffect::RequestSignature {
                    token: sign_token,
                    unsigned: Box::new(unsigned),
                }));
                None
            }
            AuthPolicyOutcome::Deny { reason } => {
                state.phase = AuthSessionPhase::Denied;
                Some((StoredAuthDenialSource::Policy, reason))
            }
            AuthPolicyOutcome::Unavailable | AuthPolicyOutcome::Error { reason: _ } => {
                state.phase = AuthSessionPhase::Error;
                None
            }
        };
        self.auth_sessions.insert(session.clone(), state);
        if let Some((source, reason)) = denial {
            self.deny_write_lanes_for_auth(&session, source, reason, &mut effects);
        }
        // A policy answer is where an AUTH session can go TERMINAL (`Denied`,
        // `Error`) — nothing further happens on that epoch, so no later wire
        // beat exists to carry its row into a diagnostics snapshot. Without
        // this marker, the phase an app's OWN refusal produced is unreachable
        // through `observe_diagnostics()` — the very surface that refusal is
        // supposed to be visible on. It is the LAZY marker: the runtime
        // coalesces it and builds no snapshot when no observer is attached.
        effects.push(Effect::DiagnosticsChanged);
        self.refresh_all_observation_evidence(&mut effects);
        effects
    }

    pub(in crate::core) fn signed_auth_matches_frozen(
        unsigned: &UnsignedEvent,
        signed: &SignedEvent,
    ) -> bool {
        unsigned.id == Some(signed.id)
            && unsigned.pubkey == signed.pubkey
            && unsigned.created_at == signed.created_at
            && unsigned.kind == signed.kind
            && unsigned.tags == signed.tags
            && unsigned.content == signed.content
            && signed.verify().is_ok()
    }

    pub(in crate::core) fn auth_source_status(state: &AuthSessionState) -> SourceStatus {
        match &state.phase {
            AuthSessionPhase::AwaitingPolicy { .. } => SourceStatus::AwaitingAuth {
                phase: AuthPhase::AwaitingPolicy,
            },
            AuthSessionPhase::AwaitingSignature { .. } => SourceStatus::AwaitingAuth {
                phase: AuthPhase::AwaitingSignature,
            },
            AuthSessionPhase::AwaitingSend { .. } | AuthSessionPhase::AwaitingOk { .. } => {
                SourceStatus::AwaitingAuth {
                    phase: AuthPhase::AwaitingRelayAck,
                }
            }
            AuthSessionPhase::Ready { .. } => SourceStatus::Requesting,
            AuthSessionPhase::Denied => SourceStatus::AuthDenied,
            AuthSessionPhase::Error => SourceStatus::Error,
        }
    }

    /// The reducer's current per-session AUTH truth, projected into the
    /// evidence vocabulary for `acquisition_evidence` (#8 U2). Sessions
    /// without an entry are the "connected but never challenged" case the
    /// evidence layer defaults to `AwaitingAuth { AwaitingChallenge }`.
    pub(in crate::core) fn auth_status_map(&self) -> BTreeMap<RelaySessionKey, SourceStatus> {
        self.auth_sessions
            .iter()
            .map(|(session, state)| (session.clone(), Self::auth_source_status(state)))
            .collect()
    }

    pub(in crate::core) fn on_auth_signer_completed(
        &mut self,
        token: AuthOpToken,
        instance: Option<AuthCapabilityInstance>,
        outcome: AuthSignerOutcome,
    ) -> Vec<Effect> {
        if !self.exact_current_auth_epoch(&token.epoch) {
            return Vec::new();
        }
        let session = token.epoch.session.clone();
        let Some(mut state) = self.auth_sessions.remove(&session) else {
            return Vec::new();
        };
        let unsigned = match &state.phase {
            AuthSessionPhase::AwaitingSignature {
                token: current,
                unsigned,
            } if *current == token => unsigned.clone(),
            _ => {
                self.auth_sessions.insert(session, state);
                return Vec::new();
            }
        };
        let missing_capability = instance.is_none()
            && state.signer_instance.is_none()
            && matches!(outcome, AuthSignerOutcome::Unavailable);
        let exact_bound = instance.is_some() && instance == state.signer_instance;
        if !missing_capability && !exact_bound {
            self.auth_sessions.insert(session, state);
            return Vec::new();
        }
        let mut effects = Vec::new();
        let denial = match outcome {
            AuthSignerOutcome::Signed(event)
                if Self::signed_auth_matches_frozen(&unsigned, &event) =>
            {
                let Some(send_token) = self.mint_auth_operation(&state.epoch) else {
                    state.phase = AuthSessionPhase::Error;
                    self.auth_sessions.insert(session, state);
                    self.refresh_all_observation_evidence(&mut effects);
                    return effects;
                };
                state.phase = AuthSessionPhase::AwaitingSend {
                    token: send_token.clone(),
                    event_id: event.id,
                    early_ok: None,
                };
                effects.push(Effect::RelayAuth(AuthEffect::Send {
                    token: send_token,
                    event: Box::new(event),
                }));
                None
            }
            AuthSignerOutcome::Rejected { reason } => {
                state.phase = AuthSessionPhase::Denied;
                Some((StoredAuthDenialSource::Signer, reason))
            }
            AuthSignerOutcome::Signed(_)
            | AuthSignerOutcome::Unavailable
            | AuthSignerOutcome::Error { .. } => {
                state.phase = AuthSessionPhase::Error;
                None
            }
        };
        self.auth_sessions.insert(session.clone(), state);
        if let Some((source, reason)) = denial {
            self.deny_write_lanes_for_auth(&session, source, reason, &mut effects);
        }
        self.refresh_all_observation_evidence(&mut effects);
        effects
    }

    pub(in crate::core) fn on_auth_capability_bound(
        &mut self,
        token: AuthOpToken,
        capability: AuthCapability,
        instance: AuthCapabilityInstance,
    ) -> Vec<Effect> {
        if !self.exact_current_auth_epoch(&token.epoch) {
            return Vec::new();
        }
        let Some(state) = self.auth_sessions.get_mut(&token.epoch.session) else {
            return Vec::new();
        };
        match (&state.phase, capability) {
            (AuthSessionPhase::AwaitingPolicy { token: current }, AuthCapability::Policy)
                if *current == token && state.policy_instance.is_none() =>
            {
                state.policy_instance = Some(instance);
            }
            (
                AuthSessionPhase::AwaitingSignature { token: current, .. },
                AuthCapability::Signer,
            ) if *current == token && state.signer_instance.is_none() => {
                state.signer_instance = Some(instance);
            }
            _ => return Vec::new(),
        }
        Vec::new()
    }

    /// Apply transport's one typed ephemeral terminal (issue #883).
    ///
    /// The reducer owns operation identity here: it matches the terminal's
    /// exact `(session, handle)` target and opaque operation token against
    /// the [`AuthOpToken`] this session is ALREADY awaiting, rather than
    /// keeping a side table of in-flight completions or trusting a token
    /// reconstructed off-thread. Anything that does not match the awaited
    /// send — a superseded epoch, a replaced generation, a completion for a
    /// different phase — is dropped without effect.
    pub(in crate::core) fn on_auth_send_completed(
        &mut self,
        completion: AuthSendCompletion,
    ) -> Vec<Effect> {
        let AuthSendCompletion {
            handle,
            session,
            operation,
            outcome,
        } = completion;
        let Some(state) = self.auth_sessions.get(&session) else {
            return Vec::new();
        };
        let AuthSessionPhase::AwaitingSend {
            token,
            event_id,
            early_ok,
        } = &state.phase
        else {
            return Vec::new();
        };
        if token.sequence != operation
            || token.epoch.handle != handle
            || token.epoch.session != session
        {
            return Vec::new();
        }
        let (event_id, early_ok) = (*event_id, early_ok.clone());
        let epoch = token.epoch.clone();
        if !self.exact_current_auth_epoch(&epoch) {
            return Vec::new();
        }
        let mut state = self
            .auth_sessions
            .remove(&session)
            .expect("the awaiting session was just read under this exact key");
        let mut effects = Vec::new();
        match outcome {
            AuthSendOutcome::Accepted => {
                if let Some((status, message)) = early_ok {
                    return self.finish_auth_ok(&session, state, event_id, status, message);
                }
                state.phase = AuthSessionPhase::AwaitingOk { event_id };
            }
            AuthSendOutcome::Unavailable => state.phase = AuthSessionPhase::Error,
        }
        self.auth_sessions.insert(session, state);
        self.refresh_all_observation_evidence(&mut effects);
        effects
    }

    pub(in crate::core) fn on_auth_capability_invalidated(
        &mut self,
        pubkey: PublicKey,
        capability: AuthCapability,
        instance: AuthCapabilityInstance,
    ) -> Vec<Effect> {
        let sessions: Vec<_> = self
            .auth_sessions
            .iter()
            .filter_map(|(session, state)| {
                let owns_instance = match capability {
                    AuthCapability::Policy => state.policy_instance == Some(instance),
                    AuthCapability::Signer => state.signer_instance == Some(instance),
                };
                (session.authenticate_as == Some(pubkey) && owns_instance)
                    .then(|| session.clone())
            })
            .collect();
        let mut effects = Vec::new();
        for session in sessions {
            if let Some(mut state) = self.invalidate_auth_epoch(&session, true, &mut effects) {
                state.phase = AuthSessionPhase::Error;
                self.auth_sessions.insert(session, state);
            }
        }
        self.refresh_all_observation_evidence(&mut effects);
        effects
    }

    pub(in crate::core) fn on_relay_connected(
        &mut self,
        handle: TransportRelayHandle,
        session: RelaySessionKey,
    ) -> Vec<Effect> {
        if self
            .slot_to_relay
            .get(&handle.slot)
            .is_some_and(|(current, _)| current.generation > handle.generation)
        {
            return Vec::new();
        }
        let mut effects = Vec::new();
        let same_physical_session = matches!(
            self.slot_to_relay.get(&handle.slot),
            Some((current, current_session)) if *current == handle && *current_session == session
        );
        let same_wire_generation =
            same_physical_session || self.session_has_live_generation(&session, handle);
        let open_failure_cleared = self.relay_open_failures.remove(&session).is_some();
        if let Some((_, displaced_session)) = self.slot_to_relay.get(&handle.slot).cloned() {
            if displaced_session != session {
                // A pool slot has one physical owner. If a newer connection
                // replaces its access context before an old disconnect arrives,
                // release the displaced session here; otherwise its AUTH epoch
                // and apparent connectivity could survive forever even though
                // no transport handle can ever make them current again.
                self.invalidate_auth_epoch(&displaced_session, false, &mut effects);
                self.connected_relays.remove(&displaced_session);
                self.auth_probe_sessions.remove(&displaced_session);
            }
        }
        // A fresh PROTECTED connection generation is NEVER pre-authorized
        // (#8): any AUTH readiness earned by an earlier generation died with
        // that socket. Sessions bound to no identity own no AUTH epoch; invalidating one
        // here would erase an initial REQ already accepted by this exact
        // still-dialing handle, then manufacture a duplicate replay (#1075).
        if session.authenticate_as.is_some() {
            self.invalidate_auth_epoch(&session, false, &mut effects);
        }
        self.slot_to_relay
            .insert(handle.slot, (handle, session.clone()));
        // A connection can also exist solely for a compiled/persisted write
        // route. It is live for the durable write scheduler and ACK
        // attribution, but it must never receive read replay/probing unless
        // the CURRENT read plan admits that exact SESSION.
        let planned_read_reqs = self.router.plan().reqs.get(&session).cloned();
        // Feeds `AcquisitionEvidence.sources[_].status` (`evidence.rs`):
        // this session is now ready to report `AwaitingRequest` for Public
        // work (or `AwaitingAuth` for protected work). It becomes
        // `Requesting` only after the exact local handoff is accepted, and is
        // never again `Connecting` for the lifetime of this `CoreState`
        // (`ever_connected_relays` is append-only -- a later drop reads
        // `Disconnected`, not `Connecting`, per the doc's "was connected,
        // then dropped" fact).
        self.connected_relays.insert(session.clone());
        self.ever_connected_relays.insert(session.clone());
        // The transport gates an identity-bound generation's ordinary
        // outbound frames until `Effect::ReleaseInitialRead`, and THIS entry
        // is what authorizes that release (`on_auth_probe_released`). It is
        // per GENERATION, so a session already known to require AUTH gets one
        // too (#1889): skipping it because an EARLIER generation was
        // challenged left the new socket gated forever — the replayed REQ
        // never reached the relay, so the relay never challenged, so
        // `finish_auth_ok` never released it. That is the same deadlock, one
        // reconnect later.
        if !same_physical_session && session.authenticate_as.is_some() {
            self.auth_probe_sessions.insert(session.clone(), handle);
        }
        // Reconnect (new generation): clear stale attribution, then rebuild
        // every currently-planned REQ for this session. The first Connected
        // callback may follow a REQ that the runtime already accepted on this
        // exact handle; that is not a reconnect and must not erase or resend
        // the accepted subscription.
        if !same_wire_generation {
            self.abandon_session_subs(&session);
        }
        // A behaviorally-proven
        // broad Public request repeats the same live-first NIP-77 handoff as
        // an ordinary recompile; it must never regress to reconcile-first or
        // silently change strategy on reconnect (#563).
        // EVERY read session replays its planned REQs at connect time, whether
        // or not it names an identity. A relay that only challenges in
        // response to a request (strfry) never speaks first, so withholding
        // the REQ until AUTH completes and starting AUTH only on an inbound
        // challenge deadlocks both sides (#1889). The REQ goes out; the relay
        // answers `CLOSED auth-required` plus `["AUTH", challenge]`; the AUTH
        // reducer's ready transition (`finish_auth_ok`) replays the full
        // planned set once this exact generation is authenticated.
        if let Some(reqs) = planned_read_reqs.as_ref() {
            // A new websocket generation has no live subscriptions even
            // if the previous generation's accepted-wire owner map still
            // names them. Fresh-generation teardown above retires that
            // state before this replay is built. NIP-77 state is relay-keyed
            // and belongs to the unauthenticated socket alone (#8), so a
            // protected generation never retires it.
            if !same_wire_generation && session.authenticate_as.is_none() {
                self.nip77.drop_live_for_relay(&session.relay);
                self.nip77.take_handoffs_probed_on_relay(&session.relay);
            }

            let mut plain_reqs = Vec::new();
            let mut handoffs = Vec::new();
            for req in reqs {
                self.install_plan_execution_metadata(
                    req.sub_id.clone(),
                    req.filter.clone(),
                    req.coverage_claims.clone(),
                    req.owner_demands.clone(),
                    req.provenance.iter().map(|fact| fact.lane).collect(),
                );
                if self.wire_request_is_live(&session, &req.sub_id, &req.filter, handle) {
                    continue;
                }
                // The live-first NIP-77 handoff is PUBLIC-session-only (#8):
                // the probe verdict was earned on the unauthenticated socket
                // and proves nothing about an authenticated session's view.
                // `query.rs`'s ordinary-recompile path carries the identical
                // predicate.
                if req.filter.limit.is_none() && session.authenticate_as.is_none() {
                    if let Some(probed) = self.prober.probed(&session.relay) {
                        handoffs.push((probed, req.sub_id.clone(), req.filter.clone()));
                        continue;
                    }
                }
                self.record_observed_request(RequestSend {
                    session: &session,
                    sub_id: &req.sub_id,
                    filter: &req.filter,
                    coverage_claims: req.coverage_claims.clone(),
                    owner_demands: req.owner_demands.clone(),
                    lanes: req.provenance.iter().map(|fact| fact.lane).collect(),
                    replay: true,
                    event_failure_target: EventFailureTarget::ThisSend,
                });
                plain_reqs.push(req.clone());
            }
            // Keep the replay boundary even when every planned request is
            // already live on this exact handle. The runtime preserves
            // its empty NMP reconnect preamble without resending anything.
            effects.push(Effect::Replay(
                session.clone(),
                self.attempted_replay(&session, plain_reqs),
            ));
            for (probed, sub_id, filter) in handoffs {
                self.begin_neg_handoff(probed, sub_id, None, filter, &mut effects);
            }
        }
        // NIP-11 is one-shot HTTP evidence, not a stream. Resolve it off the
        // reducer thread before deciding whether a behavioral NIP-77 probe
        // is useful. Explicit negative advertisement can avoid known-noisy
        // probes; positive advertisement can NEVER mint `ProbedRelay`.
        // A connection outside the current read plan has no authority to
        // create either acquisition or capability-probe work.
        if planned_read_reqs.is_some() {
            effects.push(Effect::FetchRelayInformation(session.relay.clone()));
        }
        let mut request_dispatch = Vec::new();
        let staged = std::mem::take(&mut effects);
        for effect in staged {
            if matches!(effect, Effect::Wire(_) | Effect::Replay(_, _)) {
                request_dispatch.push(effect);
            } else {
                effects.push(effect);
            }
        }
        // A relay coming online can flip a handle's `AcquisitionEvidence`
        // (`Connecting` -> `AwaitingRequest`) with no coverage/row change at
        // all. Refresh before dispatch so the accepted callback can only move
        // that fact forward to `Requesting`, never regress it afterward.
        self.refresh_all_observation_evidence(&mut effects);
        self.refresh_all_history_evidence(&mut effects);
        effects.extend(request_dispatch);
        effects.extend(self.wake_relay_lanes(&session, false));
        if open_failure_cleared {
            effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
        }
        effects
    }

    pub(in crate::core) fn on_auth_probe_released(
        &mut self,
        handle: TransportRelayHandle,
        session: RelaySessionKey,
    ) -> Vec<Effect> {
        if !self
            .slot_to_relay
            .get(&handle.slot)
            .is_some_and(|(current, current_session)| {
                *current == handle && *current_session == session
            })
        {
            return Vec::new();
        }
        if self.auth_ready_sessions.get(&session) == Some(&handle) {
            return vec![Effect::ReleaseInitialRead(handle)];
        }
        if self.auth_probe_sessions.get(&session) != Some(&handle) {
            return Vec::new();
        }
        self.auth_probe_sessions.remove(&session);
        let mut effects = vec![Effect::ReleaseInitialRead(handle)];
        effects.extend(self.wake_relay_lanes(&session, true));
        effects
    }

    pub(in crate::core) fn on_relay_information_resolved(
        &mut self,
        url: RelayUrl,
        information: Option<RelayInformationCapabilityEvidence>,
    ) -> Vec<Effect> {
        let advertises_nip77 = information
            .as_ref()
            .and_then(|information| information.supported_nips.as_ref())
            .map(|nips| nips.contains(&77));
        // NIP-11/NIP-77 capability evidence belongs to the PUBLIC session
        // only (#8): the document is unauthenticated HTTP and the probe runs
        // over the unauthenticated socket, so a URL planned solely under
        // protected sessions retains no document and probes nothing.
        let public_session = RelaySessionKey::unauthenticated(url.clone());
        let planned = self.router.plan().reqs.contains_key(&public_session);
        // The PLANNING-relevant projection of this document, before and
        // after (#931). NIP-11 acquisition is driven by connect, and connect
        // happens after the compile that planned the relay -- so an
        // advertised `max_subscriptions` is ALWAYS learned after a compile
        // that could not consult it. Without a replan here an enforced
        // budget would sit inert until some unrelated demand mutation
        // happened along, which is advisory behavior wearing an enforced
        // label.
        let budget_before = self.advertised_planning_limits(&url);
        if planned {
            if let Some(information) = information {
                self.nip11_information.insert(url.clone(), information);
            } else {
                // `None` means the service has no last-good authority for
                // this relay. An older reducer copy must not survive it.
                self.nip11_information.remove(&url);
            }
        } else {
            // A flight may complete after demand changed. Late evidence has
            // no current diagnostics owner and is never retained.
            self.nip11_information.remove(&url);
        }
        let mut effects = Vec::new();
        // Replan only when the numbers the PLANNER reads actually moved. A
        // 304 revalidation, a changed `name`, or `supported_nips` churn must
        // not cost a full recompile -- and this cannot loop, because
        // resolution is driven by connect rather than by planning.
        if self.advertised_planning_limits(&url) != budget_before {
            // A budget that binds REFUSES demand, and the app has to hear
            // that from its own query rather than from a diagnostics screen:
            // the refused atoms are `limited` in the new plan, which
            // `acquisition_evidence` renders as `ShortfallFact::LocalLimit`.
            // `recompile` already ends with its own full observation/history
            // sweep (#1646) that picks up exactly that shortfall -- a second
            // sweep here would just re-walk every live observation and
            // history a second time over a diff `recompile` already settled,
            // the same redundant-call shape #1646 deleted from
            // `on_set_active_pubkey`.
            self.recompile(&mut effects);
        }
        if self.connected_relays.contains(&public_session)
            && self.router.plan().reqs.contains_key(&public_session)
            && advertises_nip77 != Some(false)
        {
            if let Some(probe) = self.prober.begin_probe(&url) {
                let attempt_id = self.attempts.mint(RequestAttemptState {
                    session: public_session,
                    sub_id: probe.sub_id.clone(),
                    filter_hash: probe.filter.hash(),
                    filter: probe.filter.clone(),
                    coverage_claims: BTreeSet::new(),
                    owner_demands: BTreeSet::new(),
                    // A protocol-support probe is not a routed acquisition:
                    // no lane asked for it, so it reports none.
                    lanes: BTreeSet::new(),
                    replay: false,
                    event_failure_target: EventFailureTarget::ThisSend,
                    request_revision: None,
                    retry_failures: 0,
                    purpose: RequestAttemptPurpose::Nip77Probe,
                });
                effects.push(Effect::StartProbe(
                    attempt_id,
                    url,
                    probe.sub_id,
                    probe.filter,
                    probe.initial_message_hex,
                ));
            }
        }
        // Capability evidence is itself observable diagnostics state; do
        // not wait for an unrelated query recompile/EOSE to publish it.
        effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
        effects
    }

    /// `reason` is the one piece of information issue #506's CRITICAL fix
    /// restores across the pool->engine boundary. Ordinary (transient)
    /// disconnects keep EXACTLY today's behavior: the pool itself is already
    /// redialing on its own backoff schedule, and an ensure effect here
    /// is an idempotent no-op nudge for that same worker. A
    /// `DisconnectReason::PermanentlyFailed` slot is different in kind: the
    /// transport pool has ALREADY retired that worker thread for good (see
    /// `nmp_transport::DisconnectReason::PermanentlyFailed`'s doc) -- it will
    /// never redial on its own, so re-issuing `EnsureRelay` unconditionally
    /// here would either be a silent no-op racing a wedged zombie (the
    /// pre-#506 bug) or, once the pool immediately reopens on ANY
    /// `ensure_open`, a tight redial loop against a relay that keeps
    /// rejecting the same way (a 401 busy-loop -- exactly what the fix must
    /// NOT introduce). So a permanent reason records a terminal degraded
    /// fact instead (reusing the same `transport_degraded` diagnostics field
    /// `on_relay_health` already owns) and stops short of `EnsureRelay`;
    /// every other reaction below (clearing attribution, suspending
    /// in-flight write lanes, dropping open reconciliations, clearing
    /// `connected_relays`) is identical for both reasons, because the relay
    /// is equally not-connected either way. Recovery for a permanently-failed
    /// relay is still possible afterward -- an explicit demand re-add or the
    /// write scheduler's own lane demand issues a FRESH `EnsureRelay`, which the pool
    /// grants a fresh generation for because its worker slot is already
    /// empty (`ensure_open` on an empty slot is indistinguishable from
    /// `close`-then-`ensure_open`) -- it is simply never AUTOMATIC.
    pub(in crate::core) fn on_relay_disconnected(
        &mut self,
        handle: TransportRelayHandle,
        reported_session: RelaySessionKey,
        reason: DisconnectReason,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some((current, session)) = self.slot_to_relay.get(&handle.slot).cloned() {
            // Exact (handle, session) match or nothing (#8): a delayed old
            // disconnect for a superseded generation, or one reported for a
            // session that no longer occupies this slot, must not tear down
            // the session that actually lives there now.
            if current != handle || session != reported_session {
                return effects;
            }
            self.close_requests_for_session(
                &session,
                handle,
                format!("transport disconnected: {reason:?}"),
                &mut effects,
            );
            // A coordinate answer is a fact about one relay SESSION, and any
            // question still outstanding on this one died with the socket.
            // Releasing it here rather than waiting for the publish
            // scheduler to notice is what keeps a lane from parking forever
            // on a request that can no longer be answered.
            self.release_coordinate_coverage_for_relay(&session.relay, &mut effects);
            // AUTH truth is a property of the exact connection generation
            // that earned it (#8) — it dies with the socket, unconditionally,
            // for every disconnect reason: the epoch is cancelled, protected
            // lanes park, and readiness is revoked.
            self.invalidate_auth_epoch(&session, false, &mut effects);
            self.abandon_session_subs(&session);
            self.abandon_request_replacements_for_session(&session);
            self.suspend_disconnected_lanes(&session, &mut effects);
            // Negentropy (probe, live reconciliations, one-shot backfills)
            // is PUBLIC-session-only work (#8), so its teardown fires only
            // when the session bound to no identity itself dropped -- a protected
            // session's disconnect must not kill a reconciliation still
            // healthy on the URL's live Public socket.
            if session.authenticate_as.is_none() {
                self.nip77.drop_live_for_relay(&session.relay);
                // Any reconciliation open against this relay dies with the
                // connection -- there is nothing left to `NEG-CLOSE` (the
                // socket is already gone), so this is a silent drop, not a
                // fallback REQ: the relay's own `Supported` verdict stays
                // cached, and the NEXT `recompile()`/reconnect naturally
                // re-opens whatever demand still wants this shape.
                self.nip77.take_sessions_on_relay(&session.relay);
                self.nip77.take_handoffs_on_relay(&session.relay);

                // One-shot repair REQs are reducer-owned even though the
                // router never planned them. Remove their state when the
                // socket generation dies. The socket is already gone, so the
                // CLOSE is bookkeeping rather than a wire expectation.
                let stale_temporary: Vec<SubId> = self
                    .nip77
                    .take_backfills_on_relay(&session.relay)
                    .into_iter()
                    .map(|(sub_id, _)| sub_id)
                    .collect();
                for sub_id in &stale_temporary {
                    self.abandon_sub(sub_id);
                }
                if !stale_temporary.is_empty() {
                    effects.push(Effect::Wire(self.attempted_wire_delta(WireDelta {
                        ops: vec![(
                            session.clone(),
                            stale_temporary.into_iter().map(WireOp::Close).collect(),
                        )],
                    })));
                }
            }
            // Feeds `AcquisitionEvidence.sources[_].status`: this session is
            // no longer connected, but `ever_connected_relays` is untouched
            // -- a subsequent evidence computation reads `Disconnected`,
            // never `Connecting`, and any `reconciled_through` this session
            // already earned survives (the #49 "offline cached rows remain
            // usable" acceptance criterion -- watermark and link status are
            // deliberately orthogonal fields, never one enum).
            self.connected_relays.remove(&session);
            self.auth_probe_sessions.remove(&session);
            match reason {
                DisconnectReason::PermanentlyFailed => {
                    // #506: the pool already retired this worker for good --
                    // re-issuing `EnsureRelay` here would busy-loop against
                    // a relay that keeps saying no. Record the terminal
                    // degraded fact instead; recovery is only ever explicit
                    // (fresh demand or the write scheduler's lane demand).
                    let url = &session.relay;
                    self.transport_degraded = Some(format!(
                        "relay {url} permanently failed (authentication/authorization \
                         rejected) and will not automatically retry"
                    ));
                    effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
                }
                DisconnectReason::Closed => {
                    // An INTENTIONAL close (`Pool::close`) must never
                    // resurrect the session (#8/guarantee #18): the runtime's
                    // exact worker reconciliation just released it on
                    // purpose, and an unconditional `EnsureRelay` here would
                    // re-dial a still-planned session the instant it was
                    // reconciled away.
                }
                DisconnectReason::Error => {
                    // Transient drop: re-request the worker ONLY while the
                    // reducer still owns demand for exactly this session --
                    // a session no longer required must not be redialed
                    // merely because its old socket errored on the way out.
                    let required = self.relay_worker_requirements();
                    if required.writes.contains(&session) {
                        effects.push(Effect::EnsureWriteRelay(session));
                    } else if required.all.contains(&session) {
                        effects.push(Effect::EnsureReadRelay(session));
                    }
                }
            }
        }
        // Same reasoning as `on_relay_connected`: a link-status flip alone
        // must become observable via `EmitRows`.
        self.refresh_all_observation_evidence(&mut effects);
        self.refresh_all_history_evidence(&mut effects);
        effects.extend(self.schedule_ready(self.clock));
        effects
    }

    /// Consume a wire `OK` iff its event id belongs to the dedicated AUTH
    /// correlation namespace. At most one current correlation exists per
    /// admitted session. Retired ids need no tombstone set: ordinary publish
    /// structurally rejects kind:22242, so a retired/unknown AUTH id cannot
    /// exist in the durable-write correlation map and write fallback is a
    /// guaranteed no-op. Old-socket frames cannot cross a reconnect because
    /// transport handles are generation checked before this function.
    ///
    /// This is the ONE ready transition (#8): the FIRST exact-generation
    /// success records the session's planned REQs' attribution snapshots and
    /// replays them (the exact send `on_relay_connected` deliberately
    /// withheld for a protected session), wakes persisted `WaitingAuth`
    /// lanes, and refreshes evidence (`AwaitingAuth` -> `AwaitingRequest`)
    /// before dispatch. Exact local acceptance later advances it to
    /// `Requesting`; a duplicate OK for the same epoch does nothing (a second
    /// snapshot would poison the attribution FIFO with a send that never
    /// happened).
    pub(in crate::core) fn finish_auth_ok(
        &mut self,
        session: &RelaySessionKey,
        mut state: AuthSessionState,
        event_id: EventId,
        status: bool,
        message: String,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        if !status {
            state.phase = AuthSessionPhase::Denied;
            self.auth_sessions.insert(session.clone(), state);
            self.deny_write_lanes_for_auth(
                session,
                StoredAuthDenialSource::Relay,
                message,
                &mut effects,
            );
            self.refresh_all_observation_evidence(&mut effects);
            return effects;
        }

        state.phase = AuthSessionPhase::Ready { event_id };
        self.auth_ready_sessions
            .insert(session.clone(), state.epoch.handle);
        effects.push(Effect::ReleaseInitialRead(state.epoch.handle));
        if let Some(reqs) = self.router.plan().reqs.get(session).cloned() {
            for req in &reqs {
                self.install_plan_execution_metadata(
                    req.sub_id.clone(),
                    req.filter.clone(),
                    req.coverage_claims.clone(),
                    req.owner_demands.clone(),
                    req.provenance.iter().map(|fact| fact.lane).collect(),
                );
                self.record_observed_request(RequestSend {
                    session,
                    sub_id: &req.sub_id,
                    filter: &req.filter,
                    coverage_claims: req.coverage_claims.clone(),
                    owner_demands: req.owner_demands.clone(),
                    lanes: req.provenance.iter().map(|fact| fact.lane).collect(),
                    replay: true,
                    event_failure_target: EventFailureTarget::ThisSend,
                });
            }
            if !reqs.is_empty() {
                effects.push(Effect::Replay(
                    session.clone(),
                    self.attempted_replay(session, reqs),
                ));
            }
        }
        self.auth_sessions.insert(session.clone(), state);
        effects.extend(self.wake_relay_lanes(session, true));
        let replay = effects
            .iter()
            .position(|effect| matches!(effect, Effect::Replay(_, _)))
            .map(|index| effects.remove(index));
        self.refresh_all_observation_evidence(&mut effects);
        self.refresh_all_history_evidence(&mut effects);
        effects.extend(replay);
        effects
    }

    pub(in crate::core) fn on_auth_ok(
        &mut self,
        session: &RelaySessionKey,
        event_id: EventId,
        status: bool,
        message: String,
    ) -> Option<Vec<Effect>> {
        let epoch = self.auth_sessions.get(session)?.epoch.clone();
        if !self.exact_current_auth_epoch(&epoch) {
            return None;
        }
        let mut state = self.auth_sessions.remove(session)?;
        let current_event_id = match &mut state.phase {
            AuthSessionPhase::AwaitingOk { event_id: current } if *current == event_id => *current,
            AuthSessionPhase::AwaitingSend {
                token: _,
                event_id: current,
                early_ok,
            } if *current == event_id => {
                if early_ok.is_none() {
                    *early_ok = Some((status, message));
                }
                self.auth_sessions.insert(session.clone(), state);
                return Some(Vec::new());
            }
            AuthSessionPhase::Ready { event_id: current } if *current == event_id => {
                self.auth_sessions.insert(session.clone(), state);
                return Some(Vec::new());
            }
            _ => {
                self.auth_sessions.insert(session.clone(), state);
                return None;
            }
        };

        Some(self.finish_auth_ok(session, state, current_event_id, status, message))
    }

    // ---- inbound relay frame: EVENT/EOSE parsed here (D/E own OK/CLOSED/
    // NOTICE/AUTH/COUNT/NEG-*) --------------------------------------------

    #[cfg(any(
        test,
        feature = "bench-instrumentation",
        feature = "test-instrumentation"
    ))]
    pub(in crate::core) fn ingest_relay_events(
        &mut self,
        events: Vec<(SignedEvent, RelayObserved)>,
        effects: &mut Vec<Effect>,
    ) {
        self.ingest_relay_observations(
            events
                .into_iter()
                .map(|(event, observed)| (event, observed, None, None))
                .collect(),
            effects,
        );
    }

    fn ingest_relay_observations(
        &mut self,
        events: Vec<AttributedRelayObservation>,
        effects: &mut Vec<Effect>,
    ) {
        #[cfg(feature = "bench-instrumentation")]
        let call_started = std::time::Instant::now();
        #[cfg(feature = "bench-instrumentation")]
        let cpu_started = crate::ingest_attribution::thread_cpu_time_ns();
        self.ingest_relay_observations_inner(events, effects);
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::relay_ingest_observations_call(call_started.elapsed());
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::relay_ingest_observations_call_cpu(
            crate::ingest_attribution::thread_cpu_time_ns().saturating_sub(cpu_started),
        );
    }

    fn ingest_relay_observations_inner(
        &mut self,
        events: Vec<AttributedRelayObservation>,
        effects: &mut Vec<Effect>,
    ) {
        self.ingest_relay_observations_governed(events, effects, true);
    }

    pub(in crate::core) fn ingest_ordinary_relay_observations(
        &mut self,
        events: Vec<AttributedRelayObservation>,
        effects: &mut Vec<Effect>,
    ) {
        self.ingest_relay_observations_governed(events, effects, false);
    }

    fn ingest_relay_observations_governed(
        &mut self,
        events: Vec<AttributedRelayObservation>,
        effects: &mut Vec<Effect>,
        prepare_semantic_successors: bool,
    ) {
        if events.is_empty() {
            return;
        }
        #[cfg(feature = "bench-instrumentation")]
        let phase_started = std::time::Instant::now();
        #[cfg(feature = "bench-instrumentation")]
        let phase_cpu_started = crate::ingest_attribution::thread_cpu_time_ns();
        // Active semantic resources own their source/effective transition.
        // Consume those events through the one atomic store door before the
        // ordinary ingest path can make a raw relay source canonical.
        let events = if prepare_semantic_successors {
            events
                .into_iter()
                .filter(|(event, observed, candidate, attribution)| {
                    !self.install_semantic_source_successor(
                        event.clone(),
                        observed.clone(),
                        *candidate,
                        attribution.clone(),
                        effects,
                    )
                })
                .collect::<Vec<_>>()
        } else {
            events
        };
        if events.is_empty() {
            return;
        }
        let event_failure_attributions: Vec<_> = events
            .iter()
            .filter_map(|(_, _, _, attribution)| attribution.clone())
            .collect();
        // Which request delivered which replaceable/addressable coordinate
        // (#1630). Applied only after the commit succeeds, so a witness is
        // never claimed for an event this process cannot read back.
        let witnessed: Vec<_> = events
            .iter()
            .filter_map(|(event, _, _, attribution)| {
                let (session, wire_sub_id) = attribution.as_ref()?;
                let coordinate = coordinate_coverage::event_coordinate(event)?;
                Some((
                    session.clone(),
                    wire_sub_id.clone(),
                    coordinate,
                    coordinate_coverage::WitnessedCoordinate {
                        event_id: event.id,
                        created_at: event.created_at,
                    },
                ))
            })
            .collect();
        let publications: Vec<_> = events
            .iter()
            .map(|(event, observed, candidate, _)| {
                candidate.map(|candidate| {
                    CommittedObservationPublication::new(
                        observed.relay.clone(),
                        candidate,
                        event.id,
                        event.kind.as_u16(),
                    )
                })
            })
            .collect();
        let observed_events = events
            .into_iter()
            .map(|(event, observed, _, _)| (event, observed))
            .collect();
        #[cfg(feature = "bench-instrumentation")]
        {
            crate::ingest_attribution::relay_ingest_prelude(phase_started.elapsed());
            crate::ingest_attribution::relay_ingest_prelude_cpu(
                crate::ingest_attribution::thread_cpu_time_ns().saturating_sub(phase_cpu_started),
            );
        }
        // The per-session diagnostics counter (`events_by_session_kind`) is
        // bumped at the frame sites (`on_relay_frame`/`on_relay_frames`),
        // where the exact physical session is still known — a
        // `RelayObserved` carries only the URL, which cannot distinguish
        // access contexts (#8).
        #[cfg(feature = "bench-instrumentation")]
        let resolver_started = std::time::Instant::now();
        #[cfg(feature = "bench-instrumentation")]
        let resolver_cpu_started = crate::ingest_attribution::thread_cpu_time_ns();
        let resolver_result = self
            .resolver
            .ingest_observed_detailed(&mut self.store, observed_events);
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::relay_resolver_call(resolver_started.elapsed());
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::relay_resolver_call_cpu(
            crate::ingest_attribution::thread_cpu_time_ns().saturating_sub(resolver_cpu_started),
        );
        match resolver_result {
            Err(RelayIngestError::EventCommit(_error)) => {
                for (session, wire_sub_id) in event_failure_attributions {
                    self.attribution
                        .poison_event_commit_failure(&session, &wire_sub_id);
                }
            }
            Err(RelayIngestError::PostCommitProjection(_error)) => {
            }
            Ok(ingest) => {
                for (session, wire_sub_id, coordinate, witness) in witnessed {
                    self.record_coordinate_witness(&session, &wire_sub_id, coordinate, witness);
                }
                #[cfg(feature = "bench-instrumentation")]
                let phase_started = std::time::Instant::now();
                #[cfg(feature = "bench-instrumentation")]
                let phase_cpu_started = crate::ingest_attribution::thread_cpu_time_ns();
                let published = publications
                    .into_iter()
                    .zip(ingest.current_after_commit.iter().copied())
                    .filter_map(|(publication, current)| current.then_some(publication).flatten())
                    .collect::<Vec<_>>();
                // Recompute this up front from the embedded `committed.delta`
                // before it moves into `apply_committed_mutation_with` below:
                // it drives the diagnostics-vs-recompile choice, which is a
                // genuinely relay-specific concern (event counters need a
                // diagnostics beat even when the shared apply took the
                // exact/no-recompile path) and therefore stays outside the
                // one shared refresh-vs-apply decision rather than
                // re-implementing it.
                let demand_changed = !ingest.committed.delta.is_empty();
                let satisfied_pending = !ingest.satisfied_intents.is_empty();
                // An ordinary relay delivered the signed bytes an accepted
                // intent was still waiting for. The owner decides what that
                // means for the obligation's own signing state -- including
                // the local signer request that may still be outstanding for
                // it -- and hands back the receipt to settle.
                for (intent_id, canonical) in ingest.satisfied_intents {
                    if let Some(receipt_id) = self.pending.adopt_ingested_signature(intent_id) {
                        self.on_signed(receipt_id, canonical, effects);
                    }
                }
                // Ordinary committed rows do not change neutral routing
                // facts. Protocol components observe them through ordinary
                // query rows and own any fact replacement.
                if !demand_changed {
                    // Event counters are diagnostics facts even when the
                    // demand/router plan is unchanged. Preserve the prior
                    // observable update without paying a full router compile.
                    effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
                }

                #[cfg(feature = "bench-instrumentation")]
                {
                    crate::ingest_attribution::relay_ingest_post_store(phase_started.elapsed());
                    crate::ingest_attribution::relay_ingest_post_store_cpu(
                        crate::ingest_attribution::thread_cpu_time_ns()
                            .saturating_sub(phase_cpu_started),
                    );
                }
                #[cfg(feature = "bench-instrumentation")]
                let phase_started = std::time::Instant::now();
                #[cfg(feature = "bench-instrumentation")]
                let phase_cpu_started = crate::ingest_attribution::thread_cpu_time_ns();

                // A demand change may alter the capped source plan
                // and therefore evidence for otherwise-unrelated handles;
                // keep that path broad. The dominant ordinary-ingest path is
                // exact: refresh only subscriptions whose root filter matches
                // a changed row (or whose shared projection shape changed).
                // `satisfied_pending` is relay-only
                // evidence the resolver's own `delta` never carries, so they
                // ride in as explicit force flags on the SAME shared apply
                // `apply_committed_mutation` uses for every other committed-
                // mutation door, instead of re-deciding refresh-vs-apply here.
                self.apply_committed_mutation_with(
                    ingest.committed,
                    false,
                    satisfied_pending,
                    effects,
                );
                #[cfg(feature = "bench-instrumentation")]
                {
                    crate::ingest_attribution::relay_ingest_apply_committed(
                        phase_started.elapsed(),
                    );
                    crate::ingest_attribution::relay_ingest_apply_committed_cpu(
                        crate::ingest_attribution::thread_cpu_time_ns()
                            .saturating_sub(phase_cpu_started),
                    );
                }
                #[cfg(feature = "bench-instrumentation")]
                let phase_started = std::time::Instant::now();
                #[cfg(feature = "bench-instrumentation")]
                let phase_cpu_started = crate::ingest_attribution::thread_cpu_time_ns();
                if !published.is_empty() {
                    effects.push(Effect::UpdateCommittedObservations {
                        invalidated: Vec::new(),
                        published,
                    });
                }
                #[cfg(feature = "bench-instrumentation")]
                {
                    crate::ingest_attribution::relay_ingest_effect_build(phase_started.elapsed());
                    crate::ingest_attribution::relay_ingest_effect_build_cpu(
                        crate::ingest_attribution::thread_cpu_time_ns()
                            .saturating_sub(phase_cpu_started),
                    );
                }
            }
        }
    }

    pub(in crate::core) fn committed_observation_conflicts_with_pending(
        &self,
        hit: &CommittedObservationHit,
    ) -> bool {
        self.pending.values().any(|pending| {
            pending.event_id == Some(hit.event_id()) || pending.frozen.id == hit.event_id()
        })
    }

    /// Reduce a cache batch whose session, epoch, and pending-write barriers
    /// were already revalidated on this same engine thread. The ordinary
    /// `RelayFrames` door remains defensive for every unvalidated frame.
    pub(in crate::core) fn on_revalidated_committed_observations(
        &mut self,
        observations: Vec<(RelaySessionKey, u16)>,
    ) -> Vec<Effect> {
        if observations.is_empty() {
            return Vec::new();
        }
        for (session, event_kind) in observations {
            // Same as the ordinary committed-observation frame door: a
            // returned EVENT frame that names no subscription leaves no
            // request on this session with an exact returned count (#1630).
            self.erase_returned_frame_counts(&session);
            *self
                .events_by_session_kind
                .entry(session)
                .or_default()
                .entry(event_kind)
                .or_insert(0) += 1;
        }
        vec![Effect::EmitDiagnostics(self.diagnostics_snapshot())]
    }

    pub(in crate::core) fn on_relay_frames(
        &mut self,
        frames: Vec<(TransportRelayHandle, RelaySessionKey, RelayFrame)>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let mut candidates = Vec::new();
        #[cfg(feature = "bench-instrumentation")]
        let mut observed_diagnostic_duplicate = false;
        for (handle, reported_session, frame) in frames {
            let frame = match frame {
                RelayFrame::CommittedObservation(hit) => {
                    self.ingest_relay_observations(std::mem::take(&mut candidates), &mut effects);
                    let Some((current, session)) = self.slot_to_relay.get(&handle.slot).cloned()
                    else {
                        continue;
                    };
                    if current != handle || session != reported_session {
                        continue;
                    }
                    // A preparsed committed-observation hit is a returned
                    // EVENT frame carrying no subscription id, so no request
                    // on this session keeps an exact returned count (#1630).
                    self.erase_returned_frame_counts(&session);
                    *self
                        .events_by_session_kind
                        .entry(session)
                        .or_default()
                        .entry(hit.event_kind())
                        .or_insert(0) += 1;
                    continue;
                }
                frame => frame,
            };
            #[cfg(feature = "bench-instrumentation")]
            if let Some((event_kind, _)) = frame.diagnostic_duplicate_ceiling() {
                let Some((current, session)) = self.slot_to_relay.get(&handle.slot).cloned() else {
                    self.ingest_relay_observations(std::mem::take(&mut candidates), &mut effects);
                    continue;
                };
                if current != handle || session != reported_session {
                    self.ingest_relay_observations(std::mem::take(&mut candidates), &mut effects);
                    continue;
                }
                *self
                    .events_by_session_kind
                    .entry(session)
                    .or_default()
                    .entry(event_kind)
                    .or_insert(0) += 1;
                observed_diagnostic_duplicate = true;
                continue;
            }
            #[cfg(feature = "bench-instrumentation")]
            let phase_started = std::time::Instant::now();
            let observed_event = frame.into_observed_event();
            #[cfg(feature = "bench-instrumentation")]
            crate::ingest_attribution::relay_frame_conversion(phase_started.elapsed());
            match observed_event {
                Ok((subscription_id, event, candidate)) => {
                    #[cfg(feature = "bench-instrumentation")]
                    let phase_started = std::time::Instant::now();
                    let Some((current, session)) = self.slot_to_relay.get(&handle.slot).cloned()
                    else {
                        self.ingest_relay_observations(
                            std::mem::take(&mut candidates),
                            &mut effects,
                        );
                        continue;
                    };
                    // BOTH halves must match (#8): a frame carrying a stale
                    // generation OR a session that no longer occupies this
                    // slot is dropped exactly — never re-attributed to the
                    // slot's current occupant.
                    if current != handle || session != reported_session {
                        self.ingest_relay_observations(
                            std::mem::take(&mut candidates),
                            &mut effects,
                        );
                        continue;
                    }
                    #[cfg(feature = "bench-instrumentation")]
                    crate::ingest_attribution::relay_frame_session_validation(
                        phase_started.elapsed(),
                    );
                    self.record_returned_event_frame(&session, subscription_id.as_str());
                    #[cfg(feature = "bench-instrumentation")]
                    let phase_started = std::time::Instant::now();
                    *self
                        .events_by_session_kind
                        .entry(session.clone())
                        .or_default()
                        .entry(event.kind.as_u16())
                        .or_insert(0) += 1;
                    #[cfg(feature = "bench-instrumentation")]
                    crate::ingest_attribution::relay_frame_diagnostics_count(
                        phase_started.elapsed(),
                    );
                    #[cfg(feature = "bench-instrumentation")]
                    let phase_started = std::time::Instant::now();
                    candidates.push((
                        event,
                        RelayObserved::new(session.relay.clone(), self.clock),
                        candidate,
                        Some((session, subscription_id.as_str().to_string())),
                    ));
                    #[cfg(feature = "bench-instrumentation")]
                    crate::ingest_attribution::relay_frame_candidate_build(phase_started.elapsed());
                }
                Err(frame) => {
                    self.ingest_relay_observations(std::mem::take(&mut candidates), &mut effects);
                    effects.extend(self.on_relay_frame(handle, reported_session, frame));
                }
            }
        }
        self.ingest_relay_observations(candidates, &mut effects);
        #[cfg(feature = "bench-instrumentation")]
        if observed_diagnostic_duplicate {
            effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
        }
        effects
    }

    pub(in crate::core) fn on_relay_frame(
        &mut self,
        handle: TransportRelayHandle,
        reported_session: RelaySessionKey,
        frame: RelayFrame,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let msg = frame.into_message();
        let Some((current, session)) = self.slot_to_relay.get(&handle.slot).cloned() else {
            return effects; // frame from a slot we never saw RelayConnected for.
        };
        // BOTH halves must match (#8): the exact current generation AND the
        // exact session the reducer connected on this slot. A wrong-session
        // frame (however it was produced) must never consume another
        // session's attribution FIFO, coverage credit, probe, or write ack.
        if current != handle || session != reported_session {
            return effects;
        }

        match msg {
            RelayMessage::Event {
                subscription_id,
                event,
            } => {
                let event = event.into_owned();
                self.record_returned_event_frame(&session, subscription_id.as_str());
                *self
                    .events_by_session_kind
                    .entry(session.clone())
                    .or_default()
                    .entry(event.kind.as_u16())
                    .or_insert(0) += 1;
                let observed = RelayObserved::new(session.relay.clone(), self.clock);
                self.ingest_relay_observations(
                    vec![(
                        event,
                        observed,
                        None,
                        Some((session, subscription_id.as_str().to_string())),
                    )],
                    &mut effects,
                );
            }
            RelayMessage::EndOfStoredEvents(sub_id) => {
                let wire_id = sub_id.as_str();
                // Resolve before consuming the snapshot. The resolved typed
                // id routes the same EOSE into the NIP-77 handoff/repair
                // state machine after ordinary coverage attribution.
                let resolved = self.attribution.sub_id_for_wire(&session, wire_id);
                let completed = self
                    .attribution
                    .attribute_eose_detailed(&session, wire_id, self.clock);
                let completed_send = completed.as_ref().map(CompletedAttribution::send_id);
                // A `limit:0` live-first EOSE is only the barrier that opens
                // NIP-77. It terminally answers neither the original
                // question nor its reconciliation, so exposing it as
                // `RequestSettled` would let a protocol consumer derive
                // absence before NEG (and any missing-id backfill) finished.
                let opens_neg = resolved
                    .as_ref()
                    .is_some_and(|sub_id| self.nip77.is_pending_handoff(sub_id));
                let committed_coverage = completed.and_then(|completed| {
                    self.persist_attributed_completion(completed, &session.relay, &mut effects)
                });
                let settled = committed_coverage.is_some();
                let terminal_demands = if let Some(send) =
                    completed_send.filter(|_| !opens_neg && settled)
                {
                    self.emit_request_settled(send, self.clock, RequestTerminal::Eose, &mut effects)
                } else if let Some(send) = completed_send.filter(|_| !opens_neg) {
                    self.retire_request_evidence(send)
                } else {
                    BTreeSet::new()
                };
                let committed_coverage = committed_coverage.unwrap_or_default();
                self.refresh_evidence_for_coverage_and_demand_keys(
                    &committed_coverage,
                    &terminal_demands,
                    &mut effects,
                );
                if !committed_coverage.is_empty() {
                    effects.push(Effect::DiagnosticsChanged);
                }

                if let Some(resolved) = resolved {
                    let mut nip77_changed = false;
                    // This exact limited REQ is now proven active. Keep it
                    // open, overlap-close its predecessor, and only then
                    // begin Negentropy (#563).
                    if let Some(handoff) = self.nip77.take_handoff(&resolved) {
                        nip77_changed = true;
                        self.abandon_sub(&resolved);
                        self.activate_live_and_open_neg(handoff, &mut effects);
                    }

                    // Every repair REQ is one-shot and outside router-owned
                    // demand. Its EOSE closes it and either unlocks deferred
                    // NEG coverage or completes a handoff-timeout fallback.
                    if let Some(request) = self.nip77.take_backfill(&resolved) {
                        nip77_changed = true;
                        effects.push(Effect::Wire(self.attempted_wire_delta(WireDelta {
                            ops: vec![(session.clone(), vec![WireOp::Close(resolved.clone())])],
                        })));
                        self.abandon_sub(&resolved);
                        match request {
                            TemporaryReq::MissingIds {
                                plan_sub_id: _,
                                neg_sub_id,
                                attribution_send,
                                completed_at,
                            } => {
                                let committed_coverage = self.credit_neg_coverage(
                                    &neg_sub_id,
                                    attribution_send,
                                    completed_at,
                                    &session.relay,
                                    &mut effects,
                                );
                                let terminal_demands = if committed_coverage.is_some() {
                                    self.emit_request_settled(
                                        attribution_send,
                                        completed_at,
                                        RequestTerminal::Nip77,
                                        &mut effects,
                                    )
                                } else {
                                    self.retire_request_evidence(attribution_send)
                                };
                                let committed_coverage = committed_coverage.unwrap_or_default();
                                self.refresh_evidence_for_coverage_and_demand_keys(
                                    &committed_coverage,
                                    &terminal_demands,
                                    &mut effects,
                                );
                                if !committed_coverage.is_empty() {
                                    effects.push(Effect::DiagnosticsChanged);
                                }
                                self.abandon_sub(&neg_sub_id);
                            }
                            TemporaryReq::Backlog { .. } => {}
                            TemporaryReq::BacklogActivatesLive {
                                plan_sub_id,
                                live_sub_id,
                                prior_live_sub_id,
                            } => {
                                self.abandon_sub(&live_sub_id);
                                self.nip77.set_live(plan_sub_id, live_sub_id.clone());
                                if let Some(prior) = prior_live_sub_id {
                                    if prior != live_sub_id {
                                        self.abandon_sub(&prior);
                                        effects.push(Effect::Wire(self.attempted_wire_delta(
                                            WireDelta {
                                                ops: vec![(
                                                    session.clone(),
                                                    vec![WireOp::Close(prior)],
                                                )],
                                            },
                                        )));
                                    }
                                }
                            }
                        }
                    }
                    if nip77_changed {
                        effects.push(Effect::DiagnosticsChanged);
                    }
                }
            }
            RelayMessage::Ok {
                event_id,
                status,
                message,
            } => {
                // AUTH-OK correlation is checked BEFORE durable-write ACK
                // correlation (#8): the two namespaces are structurally
                // disjoint (ordinary publish rejects kind:22242), so a hit
                // here can never starve a real write ack, and a miss falls
                // through to the ordinary write path unchanged.
                if let Some(auth_effects) =
                    self.on_auth_ok(&session, event_id, status, message.clone().into_owned())
                {
                    effects.extend(auth_effects);
                } else {
                    self.handle_write_ack(
                        event_id,
                        status,
                        message.into_owned(),
                        &session,
                        &mut effects,
                    );
                }
            }
            RelayMessage::Auth { challenge } => {
                effects.extend(self.on_auth_challenge(handle, session, challenge.into_owned()));
            }
            RelayMessage::Closed { message, .. }
                if matches!(
                    message.split_once(':').map(|(prefix, _)| prefix),
                    Some("auth-required" | "restricted")
                ) =>
            {
                effects.extend(self.on_auth_restricted(handle, session));
            }
            RelayMessage::Closed {
                subscription_id,
                message,
            } => {
                let reason = message.into_owned();
                if let Some(sub_id) = self
                    .attribution
                    .sub_id_for_wire(&session, subscription_id.as_str())
                {
                    self.close_requests_for_sub(&session, handle, &sub_id, reason, &mut effects);
                }
            }
            RelayMessage::NegMsg {
                subscription_id,
                message,
            } => {
                // Negentropy is PUBLIC-session-only in this unit (#8): the
                // probe and every reconciliation were opened on the Public
                // session, so a NEG frame arriving on a protected session
                // could only be a foreign/confused reply — it must not
                // resolve the Public probe or step a Public reconciliation.
                if session.authenticate_as.is_some() {
                    return effects;
                }
                let wire_id = subscription_id.as_str();
                if self.prober.on_neg_msg(&session.relay, wire_id).is_some() {
                    // Capability probe succeeded -- the verdict is now
                    // cached (`Prober::probed`). Nothing further to do here:
                    // the NEXT `recompile()` (triggered by any future demand
                    // change) is what actually routes a broad filter for
                    // this relay onto negentropy -- see the builder report's
                    // scoping note on already-open subs at probe time.
                } else if let Some(sub_id) = self.attribution.sub_id_for_wire(&session, wire_id) {
                    self.step_neg_session(
                        sub_id,
                        session.relay.clone(),
                        message.as_ref(),
                        &mut effects,
                    );
                }
                // An unrecognized wire id is an untrusted-network fact
                // (stale/foreign sub), never a panic -- silently ignored,
                // same discipline as `handle_write_ack`'s unknown-OK case.
            }
            RelayMessage::NegErr {
                subscription_id, ..
            } => {
                // Same PUBLIC-session-only gate as `NegMsg` above (#8): a
                // protected session's NEG-ERR must not classify the URL as
                // Unsupported or tear a Public reconciliation down to REQ.
                if session.authenticate_as.is_some() {
                    return effects;
                }
                let wire_id = subscription_id.as_str();
                if self.prober.on_neg_unsupported(&session.relay, wire_id) {
                    // Probe classified Unsupported; cached, never re-probed.
                } else if let Some(sub_id) = self.attribution.sub_id_for_wire(&session, wire_id) {
                    if let Some(neg) = self.nip77.take_session(&sub_id) {
                        self.neg_session_fallback_to_req(sub_id, neg, &mut effects);
                    }
                }
            }
            // Closed (non-auth) / Notice / Count remain separate protocol
            // facts.
            _ => {}
        }
        effects
    }
}
