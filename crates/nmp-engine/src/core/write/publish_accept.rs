//! Publish acceptance through signing: intent validation, signer round-trip, and the signed-template door.

use super::*;

impl CoreState {
    /// `Publish` (issues #2/#3 U3): enter durable/at-most-once writes through
    /// `resolver.accept_local` exactly once. The store allocates both ids
    /// and commits the canonical pending row, obligation and receipt before
    /// `Accepted` is observable. Ephemeral uses the distinct receipt-only
    /// door: no pending row and no retry obligation, but still a stable,
    /// reattachable receipt as required by the promoted VISION.
    ///
    /// A `Signed` payload is verified here, at the acceptance boundary,
    /// BEFORE `WriteFact::Accepted` is ever emitted (#52 Q2). This is the
    /// only publish path in the crate — `Handle::publish` is the sole entry
    /// point regardless of caller — so verifying here, rather than at each caller,
    /// makes "a forged `Signed` event can never be published" true
    /// unconditionally instead of entry-point-dependent. A failed verify is
    /// a whole-intent terminal (`WriteFact::Failed`): no `Accepted`, no
    /// pending write recorded, no `Effect::PublishEvent`.
    ///
    /// Identity resolution (#47): a builder payload carries no author, so
    /// the identity SELECTS one and there is nothing to compare it against
    /// — `Identity::Active` resolves the current account (fail
    /// closed pre-acceptance when none is current, since nothing is pinned
    /// so nothing may park), `Identity::Explicit(pk)` stamps `pk`
    /// regardless of the current account, including while logged out. A
    /// `Signed` payload states its author in its own bytes, so there the
    /// identity may only RESTATE it: `Explicit(pk)` naming that author is a
    /// harmless restatement of consent and naming anybody else fails closed
    /// with no `Accepted`, while `Active` means the event's own author and
    /// imposes no current-account requirement at all. Acceptance pins the
    /// resolved key (`expected_pubkey` /
    /// `signing_identity_ref`), so everything downstream — the frozen body,
    /// `RequestSign`, the `SignerAttached` re-arm, restart replay — targets
    /// that one identity forever; a later `set_current_account` cannot
    /// retarget it, and an `Explicit` identity with no registered
    /// capability parks durably as `AwaitingCapability` rather than failing
    /// or drifting.
    pub(in crate::core) fn on_publish(&mut self, intent: WriteIntent) -> Vec<Effect> {
        let mut preparation = self.prepare_publish(intent);
        loop {
            match preparation {
                PublishPreparation::Complete(effects) => return effects,
                PublishPreparation::Materialize(prepared) => {
                    let PreparedReplaceableMaterialization { call, continuation } = *prepared;
                    let outcome = self.run_replaceable_materialization(call);
                    preparation =
                        self.complete_body_complete_replaceable_operation(continuation, outcome);
                }
            }
        }
    }

    pub(in crate::core) fn prepare_publish(&mut self, intent: WriteIntent) -> PublishPreparation {
        let WriteIntent {
            payload,
            routing,
            identity,
        } = intent;

        // The empty explicit route is refused FIRST, ahead of every other
        // door check: "reject it immediately". Nothing durable may exist for
        // it — no intent, no journal row, no receipt lifecycle, no signer
        // request — and it never degrades into `Auto`,
        // because sending a write to relays the caller did not choose is the
        // failure this refusal exists to prevent.
        if matches!(&routing, WriteRouting::Explicit(relays) if relays.is_empty()) {
            return PublishPreparation::Complete(
                self.refuse_publish(PublishError::EmptyExplicitRoute),
            );
        }

        let payload = match payload {
            WritePayload::ReplaceableOperation(operation) => {
                return self
                    .prepare_body_complete_replaceable_operation(operation, routing, identity)
            }
            payload => payload,
        };

        let payload_kind = match &payload {
            WritePayload::Event(builder) => builder.kind,
            WritePayload::Signed(event) => event.kind,
            WritePayload::ReplaceableOperation(_) => unreachable!("handled above"),
        };
        if payload_kind == nostr::Kind::Authentication {
            return PublishPreparation::Complete(self.refuse_publish(PublishError::ReservedKind {
                kind: payload_kind.as_u16(),
            }));
        }

        let signing_pubkey = match &payload {
            // A builder carries no author, so the identity SELECTS one —
            // there is no second source of truth for it to disagree with,
            // and the mismatch class #47 fails closed on is unrepresentable
            // here rather than merely refused.
            WritePayload::Event(_) => match identity {
                // Explicit per-write consent to publish as `pk`. The current
                // account is irrelevant (even logged out): acceptance pins
                // `pk` and downstream signing targets it forever.
                Identity::Explicit(pk) => pk,
                // Whichever account is current at acceptance. An instruction that
                // cannot resolve is a refusal, not a parked hope — nothing
                // is pinned, so nothing may park.
                Identity::Active => match self.active_pubkey {
                    Some(active) => active,
                    None => {
                        return PublishPreparation::Complete(
                            self.refuse_publish(PublishError::NoCurrentAccount),
                        )
                    }
                },
            },
            // Already-signed payloads are verified verbatim and never ask a
            // local signer, so their author is intrinsically frozen. An
            // explicit identity may still name that author (a harmless
            // restatement) — but naming anyone ELSE is a consent/author
            // contradiction and fails closed before acceptance (#47).
            WritePayload::Signed(event) => match identity {
                Identity::Explicit(pk) if pk != event.pubkey => {
                    return PublishPreparation::Complete(self.refuse_publish(
                        PublishError::IdentityContradictsSignedAuthor {
                            identity: pk,
                            author: event.pubkey,
                        },
                    ));
                }
                Identity::Explicit(_) | Identity::Active => event.pubkey,
            },
            WritePayload::ReplaceableOperation(_) => unreachable!("handled above"),
        };

        if let WritePayload::Signed(event) = &payload {
            if let Err(err) = event.verify() {
                return PublishPreparation::Complete(self.refuse_publish(
                    PublishError::SignatureInvalid {
                        reason: err.to_string(),
                    },
                ));
            }
        }

        let mut frozen = Self::freeze_payload(&payload, signing_pubkey, self.clock);

        let (id, intent_id, already_signed, accepted_signed_event, committed, retired_intents) = {
            let accept = AcceptWrite {
                payload: AcceptWritePayload::Event {
                    frozen: Box::new(frozen.clone()),
                    routing: Self::routing_snapshot(&routing),
                    // Treat an unsigned acceptance as reattachable signer work.
                    // If a signer is already present the immediate request below
                    // promotes it; if not, restart safely re-requests it.
                    sig_state: match payload {
                        WritePayload::Event(_) => IntentSigState::AwaitingSigner,
                        WritePayload::Signed(_) => IntentSigState::Pending,
                        WritePayload::ReplaceableOperation(_) => unreachable!("handled above"),
                    },
                },
                expected_pubkey: signing_pubkey,
                signing_identity_ref: signing_pubkey.to_hex(),
                accepted_at: self.clock,
            };
            let LocalAcceptResult { outcome, committed } =
                match self.resolver.accept_local(&mut self.store, accept) {
                    Ok(value) => value,
                    // Rule 1: recording anything at all needs the disk that just
                    // refused. There is no queue entry to fail into.
                    Err(err) => {
                        let effects = self.refuse_publish(PublishError::PersistenceFailed {
                            reason: err.to_string(),
                        });
                        return PublishPreparation::Complete(effects);
                    }
                };
            let Some(intent_id) = outcome.journaled_intent_id() else {
                let AcceptOutcome::Refused(reason) = outcome else {
                    unreachable!("only Refused omits journal ids")
                };
                if reason == nmp_store::RefuseReason::AlreadyExpired {
                    return PublishPreparation::Complete(
                        self.refuse_publish(PublishError::AlreadyExpired),
                    );
                }
                // CUSTODY. The store was working and said no, which is an
                // answer the app is entitled to read back — so the refusal
                // becomes a one-row, permanently-failed queue entry rather
                // than an error on the call.
                return PublishPreparation::Complete(
                    match self.store.accept_refused(frozen.id, signing_pubkey, reason) {
                        Ok(receipt_id) => {
                            let id = ReceiptId(receipt_id);
                            vec![
                                // The body this froze is the body custody
                                // holds — exactly what `accept_refused`
                                // retained above.
                                Effect::WriteAccepted(id, frozen.id),
                                Effect::EmitReceipt(
                                    id,
                                    WriteFact::Outcome(WriteOutcome::Refused(reason)),
                                ),
                            ]
                        }
                        Err(err) => self.refuse_publish(PublishError::PersistenceFailed {
                            reason: err.to_string(),
                        }),
                    },
                );
            };
            let receipt_id = outcome
                .journaled_receipt_id()
                .expect("journaled intent always has a receipt id");
            // The acceptance transaction may have moved a replaceable
            // edit's `created_at` forward against the row it CAS-ed, which
            // re-derives the id. The body it actually froze is the one
            // everything downstream must target — the signer request, the
            // pending row, the delivered bytes.
            if let Some(row) = outcome.accepted_row() {
                if row.event.id != frozen.id {
                    frozen = SignedEvent::new(
                        row.event.id,
                        row.event.pubkey,
                        row.event.created_at,
                        row.event.kind,
                        row.event.tags.clone(),
                        row.event.content.clone(),
                        sentinel_signature(),
                    );
                }
            }
            let accepted_signed_event = match &outcome {
                AcceptOutcome::Duplicate { row, .. } if row.event.sig != sentinel_signature() => {
                    Some(row.event.clone())
                }
                _ => None,
            };
            let retired_intents = match &outcome {
                AcceptOutcome::Superseded { retired, .. } => retired.clone(),
                _ => Vec::new(),
            };
            (
                ReceiptId(receipt_id),
                Some(intent_id),
                accepted_signed_event.is_some(),
                accepted_signed_event,
                Some(committed),
                retired_intents,
            )
        };

        // Acceptance IS `publish()` returning `Ok`, never a stream item: an
        // app that must ask the stream whether its write was accepted is an
        // app being made to wait on something it already knows. The same is
        // true of the identity acceptance just froze: `frozen` was already
        // re-derived above against the row the acceptance transaction CAS-ed,
        // so this is the post-restamp id and never a pre-restamp guess.
        let mut effects = vec![Effect::WriteAccepted(id, frozen.id)];

        self.pending.insert(
            id,
            PendingWrite {
                target: PendingWriteTarget::Event,
                routing,
                routing_valid: true,
                intent_id: intent_id.expect("a journaled acceptance always has an intent id"),
                // Exactly the value handed to `AcceptWrite::accepted_at`
                // above, so the in-process projection and the one a later
                // boot rebuilds from `PublishQueueIntent` are the same instant.
                accepted_at: self.clock,
                destinations_reported: false,
                signing_pubkey,
                frozen: frozen.clone(),
                already_signed,
                sign_request_in_flight: false,
                sign_generation: 0,
                event_id: None,
                pending_relays: BTreeSet::new(),
                attempt_ordinals: BTreeMap::new(),
                lane_projection: LaneWorkerProjection::default(),
                durable_routes: BTreeSet::new(),
                route_complete: false,
                route_needs: BTreeSet::new(),
            },
        );
        self.pending.remember_indexes(id, intent_id, frozen.id);

        for retired in retired_intents {
            let retired_id = ReceiptId(retired.receipt_id);
            let outcome = if retired.handoff_may_have_occurred {
                WriteOutcome::Superseded
            } else {
                WriteOutcome::NotSent(NotSentReason::Superseded)
            };
            self.emit_write_fact(retired_id, WriteFact::Outcome(outcome), &mut effects);
            if let Some(retired_pending) = self.pending.remove(&retired_id) {
                self.forget_pending_indexes(retired_id, &retired_pending, &mut effects);
            } else {
                self.pending.forget_intent(retired.intent_id);
            }
        }

        if let Some(committed) = committed {
            // A local pending row was committed before Accepted. When it did
            // not alter reactive demand/router shape, expose its exact row
            // facts through the same O(committed delta) projection path as a
            // relay batch. Any demand change keeps the broad refresh oracle.
            // Retired terminals are already queued above so a synchronous
            // new-request handoff cannot close their observer first.
            self.apply_committed_mutation(committed, &mut effects);
        }

        match payload {
            WritePayload::Event(_) => {
                if already_signed {
                    self.on_signed(
                        id,
                        accepted_signed_event
                            .expect("already-signed acceptance carries its canonical event"),
                        &mut effects,
                    );
                } else {
                    if let Some(pending) = self.pending.get_mut(&id) {
                        pending.sign_request_in_flight = true;
                        pending.sign_generation += 1;
                        let generation = pending.sign_generation;
                        // The signer signs the FROZEN body, never the
                        // builder: the author and the timestamp are decided
                        // by acceptance, so the builder is not a complete
                        // event and by construction never was one.
                        effects.push(Effect::RequestSign(
                            id,
                            generation,
                            unsigned_from_frozen(&pending.frozen),
                        ));
                    }
                }
            }
            WritePayload::Signed(event) => {
                self.on_signed(id, event, &mut effects);
            }
            WritePayload::ReplaceableOperation(_) => unreachable!("handled above"),
        }
        PublishPreparation::Complete(effects)
    }

    /// `SignerCompleted` (plan §3.4 step 2 continuation): the runtime's
    /// signer capability resolved. Explicit rejection and invalid signer
    /// output are whole-intent terminals (`WriteFact::Failed`). Transport
    /// absence, timeout, and disconnect return the retained obligation to
    /// `AwaitingCapability` so the exact frozen identity can be reattached.
    pub(in crate::core) fn on_signer_completed(
        &mut self,
        id: ReceiptId,
        generation: u64,
        result: Result<SignedEvent, SignerError>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(pending) = self.pending.get_mut(&id) else {
            return effects;
        };
        if !pending.sign_request_in_flight || pending.sign_generation != generation {
            return effects;
        }
        pending.sign_request_in_flight = false;
        match result {
            Ok(event) => self.on_signed(id, event, &mut effects),
            Err(err) => {
                if err.is_terminal() {
                    self.fail_and_compensate(id, err.to_string(), &mut effects);
                } else if let Some(pending) = self.pending.get_mut(&id) {
                    let signing_pubkey = pending.signing_pubkey;
                    let status = WriteFact::Signing(SigningState::AwaitingSigner {
                        pubkey: signing_pubkey,
                    });
                    effects.push(Effect::EmitReceipt(id, status));
                    effects.push(Effect::RearmSignerIfAvailable(signing_pubkey));
                }
            }
        }
        effects
    }

    pub(in crate::core) fn on_signer_unavailable(
        &mut self,
        id: ReceiptId,
        generation: u64,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(pending) = self.pending.get_mut(&id) {
            if !pending.sign_request_in_flight || pending.sign_generation != generation {
                return effects;
            }
            pending.sign_request_in_flight = false;
            let status = WriteFact::Signing(SigningState::AwaitingSigner {
                pubkey: pending.signing_pubkey,
            });
            effects.push(Effect::EmitReceipt(id, status));
        }
        effects
    }

    pub(in crate::core) fn on_signer_attached(&mut self, pk: PublicKey) -> Vec<Effect> {
        let mut effects = Vec::new();
        let semantic_owners = self
            .pending
            .indexed_events()
            .filter_map(|(event_id, receipts)| {
                receipts
                    .iter()
                    .filter_map(|receipt| {
                        self.pending
                            .get(receipt)
                            .filter(|pending| {
                                matches!(
                                    pending.target,
                                    PendingWriteTarget::ReplaceableOperation(_)
                                )
                            })
                            .map(|pending| (pending.intent_id, *receipt))
                    })
                    .min_by_key(|(intent, _)| *intent)
                    .map(|(_, receipt)| (*event_id, receipt))
            })
            .collect::<BTreeMap<_, _>>();
        for id in self.pending.receipt_ids() {
            let Some(pending) = self.pending.get_mut(&id) else {
                continue;
            };
            let is_physical_owner =
                !matches!(pending.target, PendingWriteTarget::ReplaceableOperation(_))
                    || semantic_owners.get(&pending.frozen.id) == Some(&id);
            if pending.signing_pubkey == pk
                && is_physical_owner
                && pending.event_id.is_none()
                && !pending.already_signed
                && !pending.sign_request_in_flight
            {
                pending.sign_request_in_flight = true;
                pending.sign_generation += 1;
                // The park ENDED, and an app told nothing keeps showing the
                // alarm the park raised (#1261). A park nobody can see end
                // is as misleading as one nobody can see start.
                effects.push(Effect::EmitReceipt(
                    id,
                    WriteFact::Signing(SigningState::InFlight { pubkey: pk }),
                ));
                effects.push(Effect::RequestSign(
                    id,
                    pending.sign_generation,
                    unsigned_from_frozen(&pending.frozen),
                ));
            }
        }
        effects
    }

    /// Commit explicit cancellation only while this receipt is still an
    /// accepted unsigned obligation. The synchronous result and emitted
    /// receipt fact come from the same reducer turn.
    pub(in crate::core) fn retained_cancel_result(
        id: ReceiptId,
        receipt: &nmp_store::PublishQueueReceipt,
    ) -> Result<CancelWriteOutcome, CancelWriteError> {
        let Some(state) = receipt.event_state() else {
            return Err(CancelWriteError::PersistenceFailed {
                receipt_id: id,
                reason: "replaceable-operation cancellation runtime is not installed".to_string(),
            });
        };
        let event_id = receipt
            .event_id()
            .expect("event receipt state and event id share one closed arm");
        match state {
            ReceiptState::Cancelled => Ok(CancelWriteOutcome::Cancelled),
            ReceiptState::Signed => Err(CancelWriteError::AlreadySigned {
                receipt_id: id,
                event_id,
            }),
            ReceiptState::Compensated => {
                Err(CancelWriteError::AlreadyCompensated { receipt_id: id })
            }
            ReceiptState::Superseded => Err(CancelWriteError::AlreadySuperseded { receipt_id: id }),
            ReceiptState::Refused(_) => Err(CancelWriteError::AlreadyRefused { receipt_id: id }),
            // Terminal already: routing finished and named nobody. There is
            // no obligation left to cancel, only an entry to remove.
            ReceiptState::NoDestination => Err(CancelWriteError::AlreadyRefused { receipt_id: id }),
            ReceiptState::Accepted => Err(CancelWriteError::PersistenceFailed {
                receipt_id: id,
                reason: "accepted receipt has no live cancellation owner".to_string(),
            }),
        }
    }

    /// Which unsigned state a still-unsigned obligation is in.
    ///
    /// One signature, one author, one answer — but two ways of not having it
    /// yet, and they are opposite advice to an app (#1261).
    /// [`SigningState::InFlight`] is a signer holding the request right now:
    /// transient, normal, and ended by the signer answering.
    /// [`SigningState::AwaitingSigner`] is nobody answering for that key at
    /// all: no clock ends it, so the app removing the entry is its only
    /// other exit. An obligation with no live row left is not waiting on any
    /// signer.
    pub(super) fn signing_park(pubkey: PublicKey, pending: Option<&PendingWrite>) -> SigningState {
        match pending {
            Some(pending) if pending.sign_request_in_flight => SigningState::InFlight { pubkey },
            _ => SigningState::AwaitingSigner { pubkey },
        }
    }

    /// Shared by the pre-signed (`on_publish`) and signer-completed paths:
    /// `Signed` -> resolve `WriteRouting` -> `Routed` -> `PublishEvent` per
    /// relay -> `Sent` per relay. Route failure (guarantee #6) is a whole-
    /// intent `Failed` with NO `PublishEvent` emitted for any relay —
    /// structurally, an unroutable private recipient cannot reach the wire
    /// here because `relays` is never bound in that branch. Every borrow of
    /// `self.pending` below is scoped to its own statement so the map can
    /// be freely read/mutated/removed across steps.
    pub(in crate::core) fn on_signed(
        &mut self,
        id: ReceiptId,
        event: SignedEvent,
        effects: &mut Vec<Effect>,
    ) {
        let Some(pending) = self.pending.get(&id) else {
            return; // unknown/already-resolved receipt id.
        };
        if pending.event_id.is_some() {
            return; // duplicate/delayed signer completion after routing.
        }

        let verified = match Self::validate_signed_template(&pending.frozen, &event) {
            Ok(verified) => verified,
            Err(reason) => {
                self.fail_and_compensate(id, reason, effects);
                return;
            }
        };

        let mut co_receipts = Vec::new();
        let mut signature_promoted = false;
        {
            let intent_id = pending.intent_id;
            let promotion_target = match &pending.target {
                PendingWriteTarget::Event => PromotionTarget::Event(intent_id),
                PendingWriteTarget::ReplaceableOperation(target) => {
                    PromotionTarget::ReplaceableMaterialization(target.clone())
                }
            };
            if !pending.already_signed {
                match self.store.promote_signed(promotion_target, verified) {
                    Ok(PromoteOutcome::Promoted { co_signed, .. }) => {
                        signature_promoted = true;
                        // The store atomically promotes every exact-duplicate
                        // co-owner against the same canonical bytes. Advance
                        // each matching in-memory obligation too; otherwise
                        // an offline co-owner could remain stranded forever
                        // behind a row that is already validly signed.
                        for co_intent in co_signed {
                            if let Some(receipt_id) = self.pending.adopt_co_signature(co_intent) {
                                co_receipts.push(receipt_id);
                            }
                        }
                    }
                    Ok(PromoteOutcome::NotFound) => {
                        self.fail_and_compensate(
                            id,
                            "accepted intent was unavailable for signature promotion".to_string(),
                            effects,
                        );
                        return;
                    }
                    Ok(PromoteOutcome::MaterializationPromoted { members, .. }) => {
                        signature_promoted = true;
                        // A semantic materialization has one physical signer
                        // request but every contributing operation owns the
                        // resulting evidence. The store promoted all member
                        // journals atomically; advance their in-memory
                        // projections without invoking promotion again.
                        for member in members {
                            if let Some(receipt_id) = self.pending.adopt_co_signature(member) {
                                if receipt_id != id {
                                    co_receipts.push(receipt_id);
                                }
                            }
                        }
                    }
                    Ok(PromoteOutcome::Stale) => {
                        // The callback names a generation that lost its CAS
                        // race. It is historical evidence only: never fail,
                        // compensate, or otherwise settle the successor.
                        return;
                    }
                    Err(err) => {
                        self.fail_and_compensate(id, err.to_string(), effects);
                        return;
                    }
                }
            }
        }

        if signature_promoted {
            match self
                .resolver
                .react_to_signature_promotion(&self.store, event.id)
            {
                Ok(committed) => self.apply_committed_mutation(committed, effects),
                // The store promotion is already committed. Preserve it and
                // degrade the projection rather than compensating a validly
                // signed obligation or manufacturing observer state.
                Err(_error) => {},
            }
        }

        let semantic_promotion = matches!(
            self.pending.get(&id).map(|pending| &pending.target),
            Some(PendingWriteTarget::ReplaceableOperation(_))
        );
        if semantic_promotion {
            for co_receipt in &co_receipts {
                if let Some(pending) = self.pending.get_mut(co_receipt) {
                    pending.event_id = Some(event.id);
                    pending.frozen = event.clone();
                }
                effects.push(Effect::EmitReceipt(
                    *co_receipt,
                    WriteFact::Signing(SigningState::Signed { event_id: event.id }),
                ));
            }
        } else {
            for co_receipt in co_receipts {
                self.on_signed(co_receipt, event.clone(), effects);
            }
        }

        if let Some(pending) = self.pending.get_mut(&id) {
            pending.event_id = Some(event.id);
            pending.frozen = event.clone();
        }

        if let Some(pending) = self.pending.get_mut(&id) {
            effects.push(Effect::EmitReceipt(
                id,
                WriteFact::Signing(SigningState::Signed { event_id: event.id }),
            ));
            if !pending.routing_valid {
                return;
            }
        }

        // Resolution moment ONE: the bytes are final, so delivery can begin.
        // It is only the FIRST opportunity, never the only one — an answer
        // that comes up short here parks and is re-executed at every later
        // moment (`resolution-lifecycle.md` §5) rather than killing a
        // durable, already-journaled obligation.
        let answer = if semantic_promotion {
            let member_receipts = self
                .pending
                .receipts_for_event(&event.id)
                .cloned()
                .unwrap_or_else(|| BTreeSet::from([id]));
            let mut answer = RouteAnswer {
                complete: true,
                ..RouteAnswer::default()
            };
            for receipt in member_receipts {
                let Some(pending) = self.pending.get(&receipt) else {
                    continue;
                };
                let member = self.resolve_routes(&pending.routing, &event);
                answer.relays.extend(member.relays);
                answer.author_route_needs.extend(member.author_route_needs);
                answer.complete &= member.complete;
            }
            answer
        } else {
            let Some(pending) = self.pending.get(&id) else {
                return;
            };
            self.resolve_routes(&pending.routing, &event)
        };

        let Some(intent_id) = self.pending.get(&id).map(|pending| pending.intent_id) else {
            return;
        };
        // A signed intent is addressable by event id whether or not routing
        // could name a single relay yet: an ack can only ever arrive for a
        // lane, and a parked intent has none, but the index must be complete
        // the moment the bytes are final so a LATER resolution's lanes need
        // no second registration step.
        self.pending.index_receipt_under_event(event.id, id);
        self.apply_route_answer(id, intent_id, answer, effects);
        if semantic_promotion {
            self.reacquire_semantic_successor_lanes(id, effects);
        }
        self.resync_route_needs(effects);
    }

    /// Freeze the body acceptance is about. This is where the fields the
    /// app left unsaid get filled in: `author` comes from identity
    /// resolution (a builder structurally cannot state one), and an unstated
    /// `created_at` is stamped `clock` — the moment the body is frozen,
    /// which is the only moment both after the app finished describing the
    /// event and before anything downstream depends on the bytes. A STATED
    /// `created_at` is kept verbatim; present-then-changed is impossible.
    pub(in crate::core) fn freeze_payload(
        payload: &WritePayload,
        author: PublicKey,
        clock: Timestamp,
    ) -> SignedEvent {
        match payload {
            WritePayload::Event(builder) => {
                let created_at = builder.created_at.unwrap_or(clock);
                // Tags reach the wire in the order the app wrote them:
                // nothing here reorders, normalises, or filters them.
                let tags = nostr::Tags::from_list(builder.tags.clone());
                SignedEvent::new(
                    EventId::new(&author, &created_at, &builder.kind, &tags, &builder.content),
                    author,
                    created_at,
                    builder.kind,
                    tags,
                    builder.content.clone(),
                    sentinel_signature(),
                )
            }
            WritePayload::Signed(event) => SignedEvent::new(
                event.id,
                event.pubkey,
                event.created_at,
                event.kind,
                event.tags.clone(),
                event.content.clone(),
                sentinel_signature(),
            ),
            WritePayload::ReplaceableOperation(_) => {
                unreachable!("body-complete operations use their dedicated acceptance branch")
            }
        }
    }

    /// The single Schnorr verification of a signer result (#387), and the
    /// only place a [`VerifiedSignature`] is minted (#768): the evidence
    /// `promote_signed` demands cannot exist without this call succeeding,
    /// so an engine that skipped this check would have nothing to hand the
    /// store door.
    pub(in crate::core) fn validate_signed_template(
        frozen: &SignedEvent,
        signed: &SignedEvent,
    ) -> Result<VerifiedSignature, String> {
        if signed.id != frozen.id
            || signed.pubkey != frozen.pubkey
            || signed.created_at != frozen.created_at
            || signed.kind != frozen.kind
            || signed.tags != frozen.tags
            || signed.content != frozen.content
        {
            return Err(
                "signer returned an event that does not match the accepted template".into(),
            );
        }
        VerifiedSignature::verify(signed)
            .map_err(|err| format!("signer returned an invalid signature: {err}"))
    }

    /// The durable spelling of a routing STRATEGY — never a resolved relay
    /// set. `Auto` journals the label alone; resolution runs fresh at every
    /// send opportunity against whatever the engine knows then.
    pub(in crate::core) fn routing_snapshot(routing: &WriteRouting) -> String {
        match routing {
            WriteRouting::Auto => "auto".to_string(),
            WriteRouting::Explicit(relays) => format!(
                "explicit-hex:{}",
                relays
                    .iter()
                    .map(|relay| hex::encode(relay.to_string()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    /// Read back a routing snapshot this build understands, or `None`.
    ///
    /// `None` is not an error path to recover from by guessing: a row spelled
    /// under a routing this build cannot read is retained exactly as written
    /// and never resolved (`routing_valid == false`). Reinterpreting a
    /// removed spelling would republish an old obligation to relays nobody
    /// chose for it, which is strictly worse than leaving it inert. Every
    /// spelling this build's own writer does not produce therefore falls here
    /// deliberately, and this decoder names none of them: asserting a dead
    /// approach is still encoding awareness of it.
    ///
    /// An `explicit-hex:` row with no relays is likewise unreadable: an empty
    /// explicit route is refused at the acceptance door, so no legitimate row
    /// can carry one.
    pub(in crate::core) fn parse_routing_snapshot(snapshot: &str) -> Option<WriteRouting> {
        if snapshot == "auto" {
            return Some(WriteRouting::Auto);
        }
        if let Some(encoded) = snapshot.strip_prefix("explicit-hex:") {
            if encoded.is_empty() {
                return None;
            }
            let relays = encoded
                .split(',')
                .map(|part| {
                    let bytes = hex::decode(part).ok()?;
                    let url = String::from_utf8(bytes).ok()?;
                    RelayUrl::parse(&url).ok()
                })
                .collect::<Option<Vec<_>>>()?;
            return Some(WriteRouting::Explicit(relays));
        }
        None
    }

    /// `publish()` refuses. Nothing durable exists and nothing ever will:
    /// there is no queue entry to inspect, nothing to retry and nothing to
    /// remove, so the caller learns it synchronously instead of being handed
    /// a receipt that will never say anything.
    pub(in crate::core) fn refuse_publish(&mut self, error: PublishError) -> Vec<Effect> {
        vec![Effect::PublishFailed(error)]
    }
}
