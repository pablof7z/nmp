//! Lane and route resolution: outbox/explicit routing, author-route needs, and route rewrite/apply.

use super::*;

/// The read path's 2-relay minimum, reused verbatim on the write side so
/// `fallback_relays`' trigger means the same thing on both
/// (`docs/internals/routing/outbox.md` §6.2).
const COVERAGE_MIN: usize = 2;

/// Which neutral direction one contributing public key uses.
#[derive(Debug, Clone, Copy)]
enum RouteDirection {
    Outbound,
    Inbound,
}

/// Every pubkey this signed event `p`-tags, in tag order, deduplicated.
///
/// Read off the SIGNED bytes, so the recipient set a resolution is evaluated
/// against is frozen for the life of the intent — which is the other half of
/// why retirement is reachable at all (the answer cannot be reopened by a tag
/// that was never in the event).
fn p_tagged_authors(event: &SignedEvent) -> BTreeSet<PublicKey> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let slice = tag.as_slice();
            let ("p", Some(value)) = (slice.first()?.as_str(), slice.get(1)) else {
                return None;
            };
            PublicKey::parse(value).ok()
        })
        .collect()
}

/// The event id this event replies to, using NMP's one thread grammar rather
/// than re-parsing `e` rows in the router.
///
/// A direct reply names its target as the thread root; a nested reply names a
/// distinct parent. In both cases the direct target is `parent.or(root)`.
/// The pointer's relay cell is intentionally ignored here: authored tag text
/// is a hint, not proof that any relay carried the referenced event.
fn reply_parent_event_id(event: &SignedEvent) -> Option<EventId> {
    if event.kind != nostr::Kind::TextNote && event.kind.as_u16() != nmp_grammar::COMMENT_KIND {
        return None;
    }
    let position = ThreadPosition::read(event);
    position
        .parent
        .or(position.root)
        .and_then(|pointer| pointer.event_id)
}

impl CoreState {
    /// Turn one intent's freshly-minted lanes into live delivery work
    /// mid-process — the counterpart of `open_bootstrapped_lanes`, which
    /// exists for lanes recovered from a PREVIOUS process and therefore has
    /// to reason about interrupted attempts this one cannot produce.
    ///
    /// A lane minted right now is always `WaitingConnection`: if the session
    /// is already up it goes straight to eligible, otherwise the receipt says
    /// so and the worker is asked for.
    pub(super) fn open_fresh_lanes(
        &mut self,
        id: ReceiptId,
        signing_pubkey: PublicKey,
        lanes: Vec<PublishQueueLane>,
        effects: &mut Vec<Effect>,
    ) {
        let write_access = Some(signing_pubkey);
        for lane in lanes {
            if matches!(lane.state, PublishQueueLaneState::WaitingConnection) {
                // The freshly-bootstrapped lane's connectivity check is
                // against the intent's identity-scoped authenticated
                // session (#8 U2), the exact session `schedule_ready` will
                // publish on.
                let session = RelaySessionKey::new(lane.key.relay.clone(), write_access);
                if self.connected_relays.contains(&session) {
                    let _ = self.commit_lane_eligible(&lane.key, lane.revision, self.clock);
                } else {
                    self.emit_write_fact(
                        id,
                        WriteFact::Relay {
                            event_id: lane.key.event_id,
                            relay: lane.key.relay.clone(),
                            state: RelayState::Waiting(RelayWaiting::NotConnected),
                        },
                        effects,
                    );
                    effects.push(Effect::EnsureWriteRelay(session));
                }
            }
        }
        effects.extend(self.schedule_ready(self.clock));
    }

    /// Execute a routing STRATEGY against what the engine knows right now.
    ///
    /// This runs on the SEND path — first attempt, boot recovery, engine
    /// tick, author-route replacement — never at compose time, so a write that parks
    /// while offline is resolved against the directory as it stands when it
    /// finally goes out, not as it stood when the app called `publish`.
    ///
    /// It is TOTAL: there is no error arm, because "the engine has not
    /// learned enough yet" is not an error — it is an `Auto` with unknowns,
    /// which is the normal INITIAL state of the queue rewriter
    /// (`docs/internals/routing/resolution-lifecycle.md` §8). A resolution
    /// that yields nothing yields `RouteAnswer::default()`, whose empty
    /// relay set and `complete == false` park the intent instead of killing
    /// it.
    ///
    /// `Auto` runs the built-in outbox derivation
    /// (`docs/internals/routing/outbox.md` §4): the event author's neutral
    /// outbound relays, operator app relays, every tagged public key's
    /// neutral inbound relays, and one verified canonical-store source for
    /// the reply parent. A settled `Absent` contributes nothing and blocks
    /// nothing; `Unknown` keeps the obligation live. A parent store-read
    /// failure returns the partial answer with `complete == false`, so
    /// already-known lanes progress while the route stays open for the next
    /// pass to re-resolve.
    ///
    /// `Explicit` never consults the directory at all: the answer is exactly
    /// the relays the caller named, nothing here adds to them, and it has no
    /// inputs and therefore no unknowns — the rewriter's fixed point,
    /// complete at its first resolution. That is guarantee #6's fail-closed
    /// discipline, kept structurally rather than by convention. The empty
    /// case is unreachable from acceptance (`on_publish` refuses it at the
    /// door); it is spelled out here only so the fail-closed answer is the
    /// one this arm can give.
    pub(in crate::core) fn resolve_routes(
        &self,
        routing: &WriteRouting,
        event: &SignedEvent,
    ) -> RouteAnswer {
        match routing {
            WriteRouting::Auto => self.resolve_outbox(event),
            WriteRouting::Explicit(relays) => RouteAnswer {
                relays: relays.iter().cloned().collect(),
                // Verbatim execution reads nothing, so nothing can still be
                // unlearned. An accepted `Explicit` is complete the instant
                // it resolves, before any relay is contacted.
                author_route_needs: BTreeSet::new(),
                complete: !relays.is_empty(),
            },
        }
    }

    /// The built-in outbox resolver — what `Auto` falls back to when no
    /// registered strategy claims the kind (`docs/internals/routing/outbox.md`).
    fn resolve_outbox(&self, event: &SignedEvent) -> RouteAnswer {
        let mut answer = RouteAnswer::default();
        let mut thin_recipient = false;
        let mut parent_provenance_error = None;

        // 1. the author's own outbox. A write fans out to EVERY write relay
        //    its author has, and one relay of my own is a fact about where I
        //    publish rather than a deficit to repair — so the author's own
        //    thinness never arms the top-up below.
        let author = event.pubkey;
        self.contribute(&author, RouteDirection::Outbound, &mut answer);

        // 2. app relays: every kind, every author, always, additive, and
        //    never counted toward the coverage minimum.
        answer
            .relays
            .extend(self.routing_facts.operator_app_relays());

        // 3. each p-tagged recipient's INBOX (read relays, never write).
        let recipients = p_tagged_authors(event);
        for recipient in &recipients {
            // Only a SETTLED answer can be short: until a recipient's list is
            // looked up to completion nobody knows what their coverage is, and
            // topping up on ignorance would widen every route the first time
            // it runs and then never narrow it. An unknown keeps the
            // resolution open, and the top-up is decided again when it lands.
            if let Some(reach) = self.contribute(recipient, RouteDirection::Inbound, &mut answer) {
                thin_recipient |= reach < COVERAGE_MIN;
            }
        }

        // 4. the reply parent's observed relay context. The event's thread
        //    grammar gives us only the referenced id; the destination comes
        //    from the canonical row's `Provenance::seen`, which proves NMP
        //    actually received that exact event from that relay. The relay
        //    hint authored into the `e` tag is never read.
        //
        //    A row can have many observations. Auto adds exactly one, using
        //    the same deterministic first-sorted verified-source policy the
        //    canonical `Row` uses when it writes a relay hint. This prevents a
        //    widely replicated parent from turning one reply into unbounded
        //    fan-out while leaving the future best-source policy to #1378.
        if let Some(parent_id) = reply_parent_event_id(event) {
            match self.store.query(&nostr::Filter::new().id(parent_id)) {
                Ok(rows) => {
                    if let Some(relay) = rows
                        .into_iter()
                        .next()
                        .and_then(|row| first_verified_source(row.provenance.seen.keys()))
                    {
                        answer.relays.insert(relay);
                    }
                }
                Err(error) => parent_provenance_error = Some(error),
            }
        }

        // Operator fallback, adopted from the read path with the read path's
        // own suppression rule: applied only when a p-tagged RECIPIENT's
        // coverage falls under the 2-relay minimum AND no app relay is
        // configured (`app_relays` suppresses fallback entirely, without
        // itself counting as coverage). The failure this closes is a reply to
        // someone whose inbound route fact names exactly one relay reaching nowhere
        // else when that relay is down; the addressee is who the minimum is
        // about.
        if thin_recipient && self.routing_facts.operator_app_relays().is_empty() {
            answer
                .relays
                .extend(self.routing_facts.operator_fallback_relays());
        }

        if answer.relays.is_empty() {
            // `complete` is a statement about KNOWLEDGE EXHAUSTION, never
            // about delivery, and a zero-destination answer is exactly where
            // that distinction earns its keep.
            //
            // Some contributor still `Unknown` means we are STILL LOOKING.
            // Nothing accumulates in that state — another day of not knowing
            // is not more evidence than the first day — so the write parks
            // indefinitely, with no cap of any kind, and every contributing
            // author stays declared as a need because a later positive
            // replacement is the only thing that can unpark it.
            //
            // Every contributor settled, and between them they named nothing,
            // means we have FINISHED looking. There is nowhere to publish,
            // and saying so is a fact rather than a guess — the write
            // terminates as `WriteOutcome::NoDestination` instead of waiting
            // forever on knowledge that has already arrived and said no.
            // (Owner ruling on #1237/#1031; this reverses the older doctrine
            // that a zero-destination answer could never retire.)
            let still_learning_authors = !answer.author_route_needs.is_empty();
            let still_learning = still_learning_authors || parent_provenance_error.is_some();
            answer.complete = !still_learning;
            if still_learning_authors {
                answer.author_route_needs.insert(author);
                answer.author_route_needs.extend(recipients);
            }
            return answer;
        }

        answer.complete = answer.author_route_needs.is_empty() && parent_provenance_error.is_none();
        answer
    }

    /// Fold one contributing author's three-valued answer into `answer`.
    ///
    /// `Some(n)` is a SETTLED answer reaching `n` relays (`Some(0)` for a
    /// definitive absence, and for a list that names nothing on the half this
    /// role wants); `None` is ignorance, which has also declared the need.
    /// The caller may only judge coverage against a settled answer — an
    /// unknown recipient has no coverage to be short of yet.
    fn contribute(
        &self,
        author: &PublicKey,
        direction: RouteDirection,
        answer: &mut RouteAnswer,
    ) -> Option<usize> {
        match self.routing_facts.author_routes(author) {
            AuthorRouteState::Present(routes) => {
                let relays = match direction {
                    RouteDirection::Outbound => routes.outbound(),
                    RouteDirection::Inbound => routes.inbound(),
                };
                answer.relays.extend(relays.iter().cloned());
                Some(relays.len())
            }
            // Settled: this input is RESOLVED, contributing nothing. It does
            // not block retirement -- that is exactly what makes the owner's
            // three-p-tag example reachable.
            AuthorRouteState::Absent => Some(0),
            // Not looked up to completion. Declare the need and keep the
            // obligation alive; the engine emits it as a neutral route need.
            AuthorRouteState::Unknown => {
                answer.author_route_needs.insert(*author);
                None
            }
        }
    }

    /// Every public key for which current read or write work needs the
    /// optional neutral author-route provider.
    ///
    /// Write needs come straight from reducer memory (`route_needs`, refreshed
    /// by the last resolution of each intent). Read needs come from the
    /// resolver's current wire-contributing `Auto` atoms whose
    /// author lacks a positive outbound route, including authors produced by
    /// a derived query. `Explicit` provider queries do not feed themselves
    /// back into this set.
    pub(in crate::core) fn author_route_needs(&self) -> BTreeSet<PublicKey> {
        let mut needs: BTreeSet<PublicKey> = self
            .pending
            .values()
            .filter(|pending| !pending.route_complete)
            .flat_map(|pending| pending.route_needs.iter().copied())
            .collect();
        needs.extend(self.author_outbox_route_needs.needs());
        needs
    }

    fn author_outbox_authors(atom: &ContextualAtom) -> Vec<PublicKey> {
        if atom.routing != ReadRouting::Auto {
            return Vec::new();
        }
        atom.filter
            .authors
            .iter()
            .flatten()
            .map(|author| {
                PublicKey::from_hex(author)
                    .expect("resolved ConcreteFilter authors are validated public keys")
            })
            .collect()
    }

    fn author_has_positive_outbox(&self, author: &PublicKey) -> bool {
        matches!(
            self.routing_facts.author_routes(author),
            AuthorRouteState::Present(routes) if !routes.outbound().is_empty()
        )
    }

    pub(in crate::core) fn retain_author_outbox_wire_owner(&mut self, atom: &ContextualAtom) {
        for author in Self::author_outbox_authors(atom) {
            let has_positive_outbox = self.author_has_positive_outbox(&author);
            self.author_outbox_route_needs
                .retain(author, has_positive_outbox);
        }
    }

    pub(in crate::core) fn release_author_outbox_wire_owner(&mut self, atom: &ContextualAtom) {
        for author in Self::author_outbox_authors(atom) {
            self.author_outbox_route_needs.release(author);
        }
    }

    /// Rebuild from the current live wire demand: reset to empty and replay
    /// through the same [`AuthorRouteNeeds::retain`] door the incremental
    /// path uses, one unit per contributed owner. There is no separate
    /// wholesale algorithm to drift from the incremental one, and no map for
    /// a caller to clear first -- the reset lives inside this method, not in
    /// `query.rs`.
    ///
    /// `finish_rebuild` (not the replay loop) decides the pending-change
    /// flag, from the exact before/after difference: an author whose route
    /// just turned positive drops out of the rebuilt set without a single
    /// `retain` call touching `needs` for that reason, so replay-time flag
    /// writes alone cannot be trusted here. See `AuthorRouteNeeds`'s module
    /// doc.
    pub(in crate::core) fn rebuild_author_outbox_route_needs(&mut self) {
        let needs_before_rebuild = self.author_outbox_route_needs.reset_for_rebuild();
        for (atom, count) in self.wire.owner_contributions() {
            for author in Self::author_outbox_authors(&atom) {
                let has_positive_outbox = self.author_has_positive_outbox(&author);
                for _ in 0..count {
                    self.author_outbox_route_needs
                        .retain(author, has_positive_outbox);
                }
            }
        }
        self.author_outbox_route_needs
            .finish_rebuild(needs_before_rebuild);
    }

    pub(in crate::core) fn flush_author_outbox_route_need_changes(
        &mut self,
        effects: &mut Vec<Effect>,
    ) {
        if self.author_outbox_route_needs.take_pending_change() {
            self.resync_route_needs(effects);
        }
    }

    /// ONE resolution moment for ONE intent: re-execute the strategy against
    /// what the engine knows right now, diff against everything this intent
    /// has ever durably resolved to, append a revision for whatever is new,
    /// and mint lanes from it through the ordinary machinery.
    ///
    /// This is the queue rewriter (`resolution-lifecycle.md` §§1-4). It is
    /// deliberately safe to run at ANY frequency: an execution that learns
    /// nothing costs a directory read and an empty diff, and the
    /// `(intent_id, relay)` lane key makes a re-reported relay collide with
    /// the lane that already exists rather than mint a second delivery
    /// obligation. Correctness never depends on which moment fired.
    ///
    /// Retired intents (`route_complete`) are skipped outright, so an `Auto`
    /// with nothing left to learn costs nothing forever after.
    pub(in crate::core) fn rewrite_route(&mut self, id: ReceiptId, effects: &mut Vec<Effect>) {
        let Some(pending) = self.pending.get(&id) else {
            return;
        };
        // Unsigned intents have no frozen recipient set to resolve against
        // yet, an unreadable routing snapshot is never resolved at all, and a
        // retired route can never change its answer again.
        if !pending.routing_valid || pending.event_id.is_none() || pending.route_complete {
            return;
        }
        let semantic = matches!(pending.target, PendingWriteTarget::ReplaceableOperation(_));
        let event = pending.frozen.clone();
        if semantic
            && self
                .pending
                .receipts_for_event(&event.id)
                .and_then(|receipts| {
                    receipts
                        .iter()
                        .filter_map(|receipt| {
                            self.pending
                                .get(receipt)
                                .map(|candidate| (candidate.intent_id, *receipt))
                        })
                        .min_by_key(|(intent, _)| *intent)
                        .map(|(_, receipt)| receipt)
                })
                != Some(id)
        {
            return;
        }
        let intent_id = pending.intent_id;
        let answer = if semantic {
            let mut answer = RouteAnswer {
                complete: true,
                ..RouteAnswer::default()
            };
            if let Some(receipts) = self.pending.receipts_for_event(&event.id) {
                for receipt in receipts {
                    let Some(member) = self.pending.get(receipt) else {
                        continue;
                    };
                    let resolved = self.resolve_routes(&member.routing, &event);
                    answer.relays.extend(resolved.relays);
                    answer
                        .author_route_needs
                        .extend(resolved.author_route_needs);
                    answer.complete &= resolved.complete;
                }
            }
            answer
        } else {
            self.resolve_routes(&pending.routing, &event)
        };
        self.apply_route_answer(id, intent_id, answer, effects);
    }

    /// Commit one [`RouteAnswer`] against an intent's durable route log and
    /// tell the receipt whatever actually changed.
    ///
    /// Emission is picture-driven, not moment-driven: a `Routed` fact is
    /// pushed only when the relay set or the `complete` flag moved, and a
    /// park is re-emitted only when its REASON changes. So a tick that
    /// learns nothing is silent on the receipt stream even though the
    /// strategy really did re-execute.
    pub(in crate::core) fn apply_route_answer(
        &mut self,
        id: ReceiptId,
        intent_id: IntentId,
        answer: RouteAnswer,
        effects: &mut Vec<Effect>,
    ) {
        let Some(pending) = self.pending.get(&id) else {
            return;
        };
        let signing_pubkey = pending.signing_pubkey;
        let _event_id = pending.frozen.id;
        // Diff-and-append: only relays absent from everything this intent has
        // ever durably resolved to are new, so an acked lane is never
        // re-minted and a resolver repeating itself writes nothing.
        let new_relays = answer
            .relays
            .difference(&pending.durable_routes)
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut union = pending.durable_routes.clone();
        let mut blocked = BTreeSet::new();
        let mut committed = false;

        if !new_relays.is_empty() {
            if self
                .commit_route_revision(intent_id, answer.relays.clone())
                .is_err()
            {
                // The route itself did not persist: these exact URLs are not
                // claimed to survive a crash, so they are owned only for this
                // process and no lane is minted for them.
                blocked = new_relays.clone();
            } else {
                committed = true;
                union.extend(answer.relays.iter().cloned());
            }
        }

        // A route revision that did not commit leaves routing INCOMPLETE, not
        // complete-with-nothing: the answer named relays, but nothing durable
        // holds them, so the write is unfinished work and the next pass
        // resolves again. Without this, an empty durable set would read as the
        // terminal `NoDestination` verdict.
        let route_complete = answer.complete && blocked.is_empty();
        let picture_changed = {
            let Some(pending) = self.pending.get_mut(&id) else {
                return;
            };
            // The waiting set is part of the picture, not decoration on it:
            // a park that stops waiting on Dave and starts waiting only on
            // Erin has told the app something new even though the relay set
            // is still empty and the resolution is still open. Leaving it out
            // of this comparison would emit the FIRST reason and then go
            // silent while the reason changed underneath the app.
            let changed = !pending.destinations_reported
                || pending.durable_routes != union
                || pending.route_complete != route_complete
                || pending.route_needs != answer.author_route_needs;
            pending.destinations_reported = true;
            pending.durable_routes = union.clone();
            pending.route_complete = route_complete;
            pending.route_needs = answer.author_route_needs.clone();
            changed
        };

        // The receipt learns WHERE before it learns what each destination is
        // doing: the routing picture is emitted ahead of any lane fact, so an
        // app never sees a relay it was never told about.
        if union.is_empty() {
            // The two empty-destination situations, kept apart by the one
            // fact that distinguishes them (#1236 dissolves here).
            //
            // `RouteAnswer::complete` is a statement about KNOWLEDGE
            // EXHAUSTION, never about delivery. So:
            //
            // - `!complete` — still learning. PARK, indefinitely, with no
            //   cap of any kind. Nothing expires it, because a user who was
            //   merely offline must never lose a message NMP never proved
            //   undeliverable. It ends when knowledge is exhausted (becoming
            //   the case below), when a route appears, or when the app
            //   removes the entry.
            // - `complete` with no blocked URL — knowledge IS exhausted and
            //   named zero relays. There is nowhere to publish, and saying so
            //   is a fact rather than a guess.
            // - a blocked URL — resolution DID name a relay, but its route
            //   revision did not commit. The durable union is still empty and
            //   the write remains open rather than inventing
            //   `NoDestination`; nothing is reported, and the next pass
            //   re-resolves.
            // `picture_changed` is the ONE authority on whether this is news:
            // it was computed against the pending state BEFORE that state was
            // updated, so re-deriving it here would compare the new value
            // with itself and silently suppress every park (the defect the
            // BDD suite caught: nine scenarios saw a receipt containing only
            // `Signing(Signed)` and nothing else).
            if picture_changed {
                self.emit_write_fact(
                    id,
                    WriteFact::Destinations {
                        relays: BTreeSet::new(),
                        complete: route_complete,
                        awaiting_author_routes: answer.author_route_needs.clone(),
                    },
                    effects,
                );
                if route_complete {
                    // Knowledge is exhausted and it named nobody. Terminal.
                    //
                    // The open-work row goes with it. This write owns no
                    // lanes at all, which is the exact precondition of
                    // `close_unroutable_intent` (the structural complement
                    // of `close_terminal_intent`'s non-empty all-terminal
                    // set), so the store can check the shape itself rather
                    // than being told a routing verdict. Leaving the row
                    // behind would strand the entry: the removal door
                    // refuses an open intent, cancellation refuses a signed
                    // one, and boot would replay it forever — on the FIRST
                    // publish of a fresh install with no reachable relay
                    // list, which is the commonest path there is.
                    //
                    // The RECEIPT is retained and reattachable either way,
                    // so the app can still read back what happened and why.
                    let closed = self.store.close_unroutable_intent(intent_id).is_ok();
                    self.emit_write_fact(
                        id,
                        WriteFact::Outcome(WriteOutcome::NoDestination),
                        effects,
                    );
                    if closed {
                        if let Some(pending) = self.pending.remove(&id) {
                            self.forget_pending_indexes(id, &pending, effects);
                        }
                    }
                }
            }
        } else {
            if picture_changed {
                self.emit_write_fact(
                    id,
                    WriteFact::Destinations {
                        relays: union.clone(),
                        complete: route_complete,
                        awaiting_author_routes: answer.author_route_needs.clone(),
                    },
                    effects,
                );
            }
        }

        if committed {
            match self.bootstrap_projected_lanes(intent_id) {
                Ok(lanes) => self.open_fresh_lanes(id, signing_pubkey, lanes, effects),
                Err(_) => {
                    // The sole call that teaches the reverse index this
                    // intent's lanes failed, so the index cannot learn what
                    // may or may not exist -- degrade rather than assume "no
                    // lanes" (epic #507 finding E5).
                }
            }
        }
    }

    /// Moment 3/4 of the lifecycle (`resolution-lifecycle.md` §5): re-execute
    /// every intent whose routing is not yet complete.
    ///
    /// Called from the engine tick as a safety net and immediately after a
    /// private author-route replacement as the latency path. The two overlap
    /// by design; because resolution is diff-and-append, running "too often"
    /// is free.
    pub(in crate::core) fn rewrite_open_routes(&mut self, effects: &mut Vec<Effect>) {
        let open = self
            .pending
            .iter()
            .filter(|(_, pending)| {
                !pending.route_complete && pending.routing_valid && pending.event_id.is_some()
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        if open.is_empty() {
            return;
        }
        for id in open {
            self.rewrite_route(id, effects);
            self.close_if_all_lanes_terminal(id, effects);
        }
        self.resync_route_needs(effects);
    }

    /// Publish a changed neutral author-route need set.
    ///
    /// A need is not a subscription. Optional protocol assembly reads this
    /// neutral set and owns any exact query it opens.
    ///
    /// This is the sole authority on whether `Effect::AuthorRouteNeedsChanged`
    /// is published: an exact diff against `last_author_route_needs`, called
    /// both directly (boot recovery, `rewrite_open_routes`) and gated by
    /// [`AuthorRouteNeeds::take_pending_change`] via
    /// `flush_author_outbox_route_need_changes`. It deliberately does not
    /// also consult or clear that flag here -- `take_pending_change` exists
    /// for exactly one caller to decide whether calling this function is
    /// worth it, not for this function to re-derive its own answer from.
    pub(in crate::core) fn resync_route_needs(&mut self, effects: &mut Vec<Effect>) {
        let current = self.author_route_needs();
        if current != self.last_author_route_needs {
            self.last_author_route_needs = current.clone();
            effects.push(Effect::AuthorRouteNeedsChanged(current));
        }
    }
}
