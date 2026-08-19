//! The composer: publish a post, show per-relay delivery state, insert
//! optimistically.
//!
//! ## What we wanted to write
//!
//! ```text
//! let sending = engine.publish(post)?;          // id known immediately
//! for state in sending.progress() { paint(state) }   // per-relay, live
//! ```
//!
//! ## What we found
//!
//! ### The suspicion about the event id is FALSE, and worth saying plainly
//!
//! `Engine::publish` returns `ReceiptStream { id, event_id, statuses }`.
//! `event_id` is the frozen NIP-01 id, decided in the same transaction that
//! issued the receipt, post-restamp in every case. Nothing has to be
//! scavenged from a later fact. `crates/nmp/src/engine/publication.rs` says so
//! and `crates/nmp-runtime/src/receipt_stream.rs` carries the field.
//!
//! ### The optimistic row is ALSO already handled, and nothing says so
//!
//! `RowSignature::Pending` is documented as "locally accepted and
//! query-visible while the exact signer is pending", and cancelling a write is
//! documented as compensating "the optimistic row the write promised". So the
//! engine inserts the row into every matching live query at acceptance. This
//! app does not insert anything and the post appears.
//!
//! Two caveats an app has to discover on its own:
//!
//! - The signal for "this is my un-sent post" is `row.signature() ==
//!   RowSignature::Pending`. That is a delivery fact wearing a cryptographic
//!   name, and it is the ONLY way to tell. There is no `row.is_local()`.
//! - `Demand::cache` defaults to `CacheMode::Agnostic`, which serves cached
//!   rows regardless of provenance, so the pending row shows up. A screen
//!   pinned with `CacheMode::Strict` -- the mode a room timeline wants, because
//!   it wants only what the host relay actually holds -- would NOT show the
//!   user's own just-written message. That trap is not documented anywhere
//!   near either type.
//!
//! ### Per-relay delivery state IS where it fights you
//!
//! There are two doors and neither is "watch this write".
//!
//! 1. `ReceiptStream::result(self)` -- consumes the stream, blocks until the
//!    ONE terminal result, and correctly handles the FIFO lagging by replaying
//!    from durable receipt storage. It throws away every intermediate fact,
//!    which is the entire content of a delivery indicator.
//!
//! 2. `stream.statuses.recv()` -- the live facts, including every
//!    `WriteFact::Relay { relay, state }`. The field's type is
//!    `FifoReceiver<WriteFact>`, `#[doc(hidden)]` in `nmp`, and its `recv`
//!    returns `Err(FifoRecvError::Lagged)` when the app is slower than the
//!    engine. Handling that means calling `Engine::reattach_receipt`,
//!    inspecting `ReceiptReattachment`, swapping in the replayed page, and
//!    following `next_cursor` -- which is `collect_receipt_result` in
//!    `nmp-runtime`, verbatim, re-written in the app. [`Delivery::drain`]
//!    below is that re-write. It is 40 lines to draw one checkmark.
//!
//! ### Joining a rendered row back to its delivery state
//!
//! A timeline row knows its `EventId`. `Engine::publish_queue_for_event(id,
//! after, limit)` is the join, and it is a PAGED query with a `u8` limit that
//! returns `Vec<PublishQueueEntry>` -- because more than one receipt can own
//! identical bytes. So the send indicator on a row is a paged store read per
//! row per repaint. [`Composer::delivery_of`] does it. What a list wants is
//! one call for the whole visible page.

use std::collections::BTreeMap;
use std::time::Duration;

use nmp::{
    Engine, EngineError, EventBuilder, EventId, Identity, Kind, PublicKey, ReceiptId,
    ReceiptStream, RelayState, RelayUrl, Row, SigningState, WriteFact, WriteIntent, WriteOutcome,
    WritePayload, WriteRouting,
};

/// The delivery state of one write, as a view model.
#[derive(Debug, Clone, Default)]
pub struct Delivery {
    pub receipt: Option<ReceiptId>,
    pub event_id: Option<EventId>,
    pub signing: Option<SigningState>,
    /// The intended destination set, and whether routing has settled. Both
    /// halves are needed: "3 of 4 relays" is meaningless without knowing
    /// whether 4 is final.
    pub intended: Vec<RelayUrl>,
    pub route_complete: bool,
    /// Who we are still waiting on author routes for. This one is genuinely
    /// good -- keys, not a sentence, so the UI can name the people.
    pub awaiting_routes: Vec<PublicKey>,
    pub per_relay: BTreeMap<String, RelayState>,
    pub outcome: Option<WriteOutcome>,
    /// Set when the live FIFO lagged and this app had to replay. Recorded
    /// because the recovery is the app's, so the failure to recover is too.
    pub replayed: bool,
}

impl Delivery {
    fn absorb(&mut self, fact: WriteFact) {
        match fact {
            WriteFact::Signing(state) => self.signing = Some(state),
            WriteFact::Relay {
                relay,
                state,
                event_id,
            } => {
                self.event_id = Some(event_id);
                self.per_relay.insert(relay.to_string(), state);
            }
            WriteFact::Destinations {
                relays,
                complete,
                awaiting_author_routes,
            } => {
                self.intended = relays.into_iter().collect();
                self.route_complete = complete;
                self.awaiting_routes = awaiting_author_routes.into_iter().collect();
            }
            WriteFact::Outcome(outcome) => self.outcome = Some(outcome),
        }
    }

    /// "2 of 3 relays" for the UI, or `None` while routing is still open.
    #[must_use]
    pub fn published_fraction(&self) -> Option<(usize, usize)> {
        if !self.route_complete {
            return None;
        }
        let published = self
            .per_relay
            .values()
            .filter(|state| matches!(state, RelayState::Published))
            .count();
        Some((published, self.intended.len()))
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.outcome.is_some()
    }
}

/// One composed post in flight.
pub struct Sending {
    stream: Option<ReceiptStream>,
    pub state: Delivery,
}

impl Sending {
    /// Pump the receipt for up to `budget`, folding every fact into `state`.
    ///
    /// This is `nmp-runtime::collect_receipt_result` rewritten in an app,
    /// minus the durable-replay cursor walk (which needs
    /// `Engine::reattach_receipt_from` and the `#[doc(hidden)]`
    /// `ReceiptReplayCursor`, so the app version simply gives up and marks
    /// itself `replayed`). A UI that renders a progress bar has no other
    /// option, because the door that DOES handle replay
    /// (`ReceiptStream::result`) consumes the stream and returns only the
    /// terminal.
    pub fn drain(&mut self, engine: &Engine, budget: Duration) {
        let deadline = std::time::Instant::now() + budget;
        loop {
            let Some(stream) = self.stream.as_ref() else {
                return;
            };
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            match stream.statuses.recv_timeout(remaining) {
                Ok(fact) => {
                    let terminal = matches!(fact, WriteFact::Outcome(_));
                    self.state.absorb(fact);
                    if terminal {
                        self.stream = None;
                        return;
                    }
                }
                Err(nmp::FifoRecvTimeoutError::Timeout) => return,
                Err(nmp::FifoRecvTimeoutError::Closed) => {
                    self.stream = None;
                    return;
                }
                Err(nmp::FifoRecvTimeoutError::Lagged) => {
                    // The engine outran us. Everything accumulated so far may
                    // be missing facts. The supported recovery is
                    // `Engine::reattach_receipt` -> `ReceiptReattachment` ->
                    // swap the receiver -> follow `next_cursor`, and
                    // `next_cursor` is only usable through the doc-hidden
                    // `reattach_receipt_from`. An app-level progress bar
                    // reconstructs itself from the queue instead.
                    self.state.replayed = true;
                    self.state.per_relay.clear();
                    if let Some(receipt) = self.state.receipt {
                        if let Some(entry) = queue_entry(engine, receipt) {
                            apply_entry(&mut self.state, &entry);
                        }
                    }
                    self.stream = None;
                    return;
                }
            }
        }
    }

    /// Block until the one terminal result, discarding intermediate facts.
    /// The supported door, and the reason `drain` exists beside it.
    pub fn settle(&mut self) -> Option<Result<nmp::ReceiptResult, nmp::ReceiptResultError>> {
        self.stream.take().map(ReceiptStream::result)
    }
}

fn queue_entry(engine: &Engine, receipt: ReceiptId) -> Option<nmp::PublishQueueEntry> {
    engine
        .publish_queue(None, u8::MAX)
        .ok()?
        .into_iter()
        .find(|entry| entry.receipt_id == receipt)
}

fn apply_entry(state: &mut Delivery, entry: &nmp::PublishQueueEntry) {
    state.event_id = Some(entry.event_id);
    state.signing = Some(entry.signing.clone());
    state.intended = entry.relays.iter().cloned().collect();
    state.route_complete = entry.route_complete;
    state.per_relay = entry
        .relay_states
        .iter()
        .map(|(relay, relay_state)| (relay.to_string(), relay_state.clone()))
        .collect();
    state.outcome = entry.outcome.clone();
}

/// The composer surface.
pub struct Composer;

impl Composer {
    /// Publish a plain text note as `author`.
    ///
    /// `Identity::Explicit` rather than `Identity::Active` because this app has
    /// two accounts live at once and the composer knows which one is typing.
    /// This is the surface working well: identity is per-write, it is frozen at
    /// acceptance, and a later account switch cannot retarget it.
    pub fn post(engine: &Engine, author: PublicKey, text: &str) -> Result<Sending, EngineError> {
        let intent = WriteIntent {
            payload: WritePayload::Event(EventBuilder::new(Kind::from(1u16)).content(text)),
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(author),
        };
        Self::send(engine, intent)
    }

    /// Reply to a row.
    ///
    /// `nmp::reply_to(&row)` is the good door: it reads the TARGET's thread
    /// position, fills the letter, the relay hint from verified provenance, the
    /// author slot and the carried mentions. It works on a pending row too,
    /// because `Row: RootScope` goes through `event_for_store()` rather than
    /// `signed_event()` -- the same reading `thread::depth_of` cannot reach.
    pub fn reply(
        engine: &Engine,
        author: PublicKey,
        parent: &Row,
        text: &str,
    ) -> Result<Sending, EngineError> {
        let builder = nmp::reply_to(parent).content(text);
        let intent = WriteIntent {
            payload: WritePayload::Event(builder),
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(author),
        };
        Self::send(engine, intent)
    }

    /// React to a row with `+`.
    ///
    /// `nmp_nip25::react` takes `&Event`, so a PENDING row cannot be reacted to
    /// -- `Row::signed_event()` is `None` and there is nothing to hand it. The
    /// user's own just-posted message is un-reactable until its signer answers.
    /// `reply_to` does not have this problem because it takes `&impl RootScope`
    /// and `Row` implements it; `nmp-nip25` and `nmp-nip18` both take `&Event`
    /// instead, so the three tagging doors disagree about what a target is.
    pub fn react(
        engine: &Engine,
        author: PublicKey,
        target: &Row,
        symbol: &str,
    ) -> Result<Sending, ReactError> {
        let event = target.signed_event().ok_or(ReactError::TargetNotSigned)?;
        let source = target.sources().iter().next().cloned();
        // `Reaction::emoji` is the validating constructor -- and
        // `Reaction::Emoji(String)` is a public variant an app can build
        // directly, bypassing the empty-string and `:shortcode:` refusals the
        // constructor exists to enforce. Two ways in, one of them wrong, and
        // the wrong one is the shorter one.
        let reaction = nmp_nip25::Reaction::emoji(symbol).map_err(ReactError::Reaction)?;
        let builder = nmp_nip25::react(&event, source, reaction);
        let intent = WriteIntent {
            payload: WritePayload::Event(builder),
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(author),
        };
        Self::send(engine, intent).map_err(ReactError::Engine)
    }

    /// Repost a row. Same `&Event` constraint as `react`.
    pub fn repost(engine: &Engine, author: PublicKey, target: &Row) -> Result<Sending, ReactError> {
        let event = target.signed_event().ok_or(ReactError::TargetNotSigned)?;
        let source = target.sources().iter().next().cloned();
        let intent = WriteIntent {
            payload: WritePayload::Event(nmp_nip18::repost(&event, source)),
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(author),
        };
        Self::send(engine, intent).map_err(ReactError::Engine)
    }

    fn send(engine: &Engine, intent: WriteIntent) -> Result<Sending, EngineError> {
        let stream = engine.publish(intent)?;
        let state = Delivery {
            receipt: Some(stream.id),
            event_id: Some(stream.event_id),
            ..Delivery::default()
        };
        Ok(Sending {
            stream: Some(stream),
            state,
        })
    }

    /// The send indicator for one already-rendered row.
    ///
    /// One paged store read per row. `limit` is a `u8`, and the result is a
    /// `Vec` because several receipts can own identical bytes, so an app that
    /// wants one indicator picks one entry and hides the rest -- exactly what
    /// the API refused to do for it, now done worse, in the view layer.
    #[must_use]
    pub fn delivery_of(engine: &Engine, event_id: EventId) -> Option<Delivery> {
        let entry = engine
            .publish_queue_for_event(event_id, None, 4)
            .ok()?
            .into_iter()
            .next()?;
        let mut state = Delivery {
            receipt: Some(entry.receipt_id),
            ..Delivery::default()
        };
        apply_entry(&mut state, &entry);
        Some(state)
    }
}

#[derive(Debug)]
pub enum ReactError {
    /// The target row is locally accepted and unsigned, so `nmp-nip25` /
    /// `nmp-nip18` cannot be handed it.
    TargetNotSigned,
    Reaction(nmp_nip25::ReactionError),
    Engine(EngineError),
}

impl std::fmt::Display for ReactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TargetNotSigned => {
                f.write_str("the target row has no signature yet, so it cannot be pointed at")
            }
            Self::Reaction(error) => write!(f, "{error:?}"),
            Self::Engine(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ReactError {}
