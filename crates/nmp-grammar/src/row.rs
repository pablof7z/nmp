//! The canonical query-result row (#105/#1707): the read-side value
//! counterpart to [`crate::WriteIntent`]. This crate holds value types only
//! (see the crate doc), and `Row`/`RowSignature`/`RowDelta` are exactly
//! that -- pure projections over `nostr` types, with no engine, store, or
//! router access anywhere in their own methods. They lived in `nmp`'s
//! reducer module by convenience rather than necessity; #1707 moved them
//! here to sit beside the write vocabulary they mirror.
//!
//! Two constructors stay OUT of this module on purpose:
//! `Row::from_stored_event`/`RowSignature::from_store` named `nmp-store`'s
//! `SigState` directly, and `nmp-store` already depends on `nmp-grammar` --
//! moving them here would be a package cycle. Both are pure translations
//! expressible through [`Row::from_parts`], so `nmp` rebuilds them as free
//! functions instead of methods, and nothing here loses capability for it.

use std::collections::BTreeSet;

use nostr::secp256k1::schnorr::Signature;
use nostr::{EventId, PublicKey, RelayUrl, Timestamp, UnsignedEvent};

use crate::tagging::{event_parent_rows, event_root_rows, RootScope, TagOptions};

/// The sentinel signature every pending row's frozen body carries until a
/// real one is promoted in: a NIP-01 id is `hash([0,pubkey,created_at,kind,
/// tags,content])` -- the signature is not an id input -- so an all-zero
/// 64-byte value round-trips through `nostr::Event`/JSON/`Filter::
/// match_event` unverified (schnorr `Signature` parsing is length-checked
/// only) and the id is final before a real signature exists. Shared, one
/// spelling: `nmp-store` re-exports this exact function rather than
/// defining its own, so the store and `Row::event_for_store` can never
/// disagree about what the sentinel is.
#[must_use]
pub fn sentinel_signature() -> Signature {
    Signature::from_slice(&[0u8; 64])
        .expect("64 zero bytes is always a structurally valid (length-checked) schnorr signature")
}

/// The one owner of a canonical row's signature state.
///
/// A locally accepted optimistic row has its final id and body before its
/// signer answers. Keeping state and signature in one closed value makes both
/// invalid combinations unrepresentable at the public boundary: a pending
/// row cannot carry a signature, and a signed row cannot omit one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowSignature {
    /// Locally accepted and query-visible while the exact signer is pending.
    Pending,
    /// Carries signature bytes. Rows emitted by NMP reach this arm only after
    /// relay verification or signer promotion; raw `Row::from_parts` values
    /// make no validity claim until an app inserts them through a verified
    /// door.
    Signed(Signature),
}

/// The canonical row value (#105): the unsigned event body, its closed
/// signature value, and its sorted, deduplicated relay-observation set --
/// `nmp_store::Provenance::seen`'s keys, projected honestly rather than
/// mirrored into a second parallel provenance store.
/// `sources` only ever grows for a given event id (`Provenance::
/// merge_observation` never removes an entry), so `Row`/`RowDelta` never
/// need a "sources shrank" case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub(crate) body: UnsignedEvent,
    pub(crate) signature: RowSignature,
    pub sources: BTreeSet<RelayUrl>,
}

/// The one current policy for choosing one relay from verified provenance.
///
/// Both an emitted reference hint and `nmp`'s Auto reply-parent lane
/// (`core/write.rs`, over `nmp-store`'s own provenance set, not this
/// crate's `Row`) use this same door, so the two consumers can never drift
/// into different answers. #1243's tagging-door record deliberately
/// deferred source ranking; #1378 owns the future best-source policy.
/// Sources are already normalized ordered collections, so the first entry
/// is deterministic. Public because both callers exist on opposite sides
/// of the #1707 package boundary.
pub fn first_verified_source<'a>(
    sources: impl IntoIterator<Item = &'a RelayUrl>,
) -> Option<RelayUrl> {
    sources.into_iter().next().cloned()
}

impl Row {
    /// Construct a row-shaped value without asserting that NMP observed it.
    ///
    /// This is the raw composition/preview/import door used by native
    /// adapters. It does not insert into NMP or claim provenance or signature
    /// validity. The closed `signature` value still guarantees that pending
    /// carries no signature and signed always carries one.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        id: EventId,
        pubkey: PublicKey,
        created_at: Timestamp,
        kind: nostr::Kind,
        tags: nostr::Tags,
        content: String,
        signature: RowSignature,
        sources: BTreeSet<RelayUrl>,
    ) -> Self {
        Self {
            body: UnsignedEvent {
                id: Some(id),
                pubkey,
                created_at,
                kind,
                tags,
                content,
            },
            signature,
            sources,
        }
    }

    /// Construct a fully-signed row directly from a verified relay event.
    /// Fixture/test convenience -- production code builds a `Row` from
    /// `nmp-store` state through `nmp`'s own `row_from_stored_event`, which
    /// also has to represent the `Pending` case this door cannot express.
    pub fn from_relay_event(event: nostr::Event, sources: BTreeSet<RelayUrl>) -> Self {
        let signature = RowSignature::Signed(event.sig);
        Self {
            body: UnsignedEvent {
                id: Some(event.id),
                pubkey: event.pubkey,
                created_at: event.created_at,
                kind: event.kind,
                tags: event.tags,
                content: event.content,
            },
            signature,
            sources,
        }
    }

    /// Final NIP-01 event id, available before signing because signatures are
    /// not part of the id preimage.
    pub fn id(&self) -> EventId {
        self.body
            .id
            .expect("a canonical row always carries its frozen event id")
    }

    pub fn pubkey(&self) -> PublicKey {
        self.body.pubkey
    }

    pub fn created_at(&self) -> Timestamp {
        self.body.created_at
    }

    pub fn kind(&self) -> nostr::Kind {
        self.body.kind
    }

    pub fn tags(&self) -> &nostr::Tags {
        &self.body.tags
    }

    pub fn content(&self) -> &str {
        &self.body.content
    }

    pub fn signature(&self) -> RowSignature {
        self.signature
    }

    /// Update this row's signature state in place, e.g. signature promotion
    /// over an already-remembered row. Body and sources are untouched;
    /// `RowSignature` stays a closed value, so this cannot desync a pending
    /// row from carrying a signature or vice versa. Needs `signature`'s
    /// private field, so it lives on `Row` itself rather than as a free
    /// function in `nmp` alongside `row_from_stored_event`.
    pub fn set_signature(&mut self, signature: RowSignature) {
        self.signature = signature;
    }

    pub fn sources(&self) -> &BTreeSet<RelayUrl> {
        &self.sources
    }

    /// Reconstruct a complete NIP-01 event value only when signature bytes are
    /// present. Pending rows return `None`; NMP's internal storage sentinel is
    /// never exposed through this door. This does not independently verify a
    /// raw `Row::from_parts` value.
    pub fn signed_event(&self) -> Option<nostr::Event> {
        let RowSignature::Signed(signature) = self.signature else {
            return None;
        };
        Some(nostr::Event::new(
            self.id(),
            self.body.pubkey,
            self.body.created_at,
            self.body.kind,
            self.body.tags.clone(),
            self.body.content.clone(),
            signature,
        ))
    }

    /// Rebuild the store's legacy event-shaped representation. Pending rows
    /// receive the store sentinel only for the duration of an internal
    /// mechanism call; the sentinel is not part of `Row`'s documented public
    /// contract and `signed_event` is the door an app should use. Kept
    /// `pub` (not `pub(crate)`) because `nmp`'s own runtime and protocol
    /// doors, in a different crate than this one after #1707, need it too --
    /// not because it is meant for general app use.
    pub fn event_for_store(&self) -> nostr::Event {
        let signature = match self.signature {
            RowSignature::Pending => sentinel_signature(),
            RowSignature::Signed(signature) => signature,
        };
        nostr::Event::new(
            self.id(),
            self.body.pubkey,
            self.body.created_at,
            self.body.kind,
            self.body.tags.clone(),
            self.body.content.clone(),
            signature,
        )
    }

    /// The relay hint a reference row to this event carries.
    ///
    /// `sources` is **verified** provenance: NMP observed this exact event at
    /// those relays, and since #1221 the set means "relays that hold it", not
    /// "whatever delivered it first". That is the honest thing to put in a
    /// hint slot that has, across the entire tree before #1243, been filled
    /// exactly once.
    ///
    /// Which of several verified sources is the BEST hint is deliberately
    /// still open (#1243's design record, "where relay hints come from"). The
    /// better answer than either single fact is a relay present in both the
    /// seen set and the author's declared NIP-65 outbox — welshman prefers
    /// declared for staleness reasons, quartz tracks nothing and takes hints
    /// from the caller, and NMP is unusual in holding both facts. That
    /// computation needs NIP-65, which `nmp-grammar` cannot reach, so it
    /// belongs at the publish door or in the app rather than folded in here.
    /// Until it exists this is the first source in sorted order, which is
    /// deterministic rather than arbitrary; an app that knows better states
    /// its own with `from_relay`.
    fn verified_hint(&self) -> Option<RelayUrl> {
        first_verified_source(&self.sources)
    }
}

#[cfg(test)]
mod row_signature_tests {
    use super::*;
    use nostr::Keys;

    fn signed_event() -> nostr::Event {
        nostr::EventBuilder::text_note("one signature owner")
            .sign_with_keys(&Keys::generate())
            .expect("fixture signs")
    }

    #[test]
    fn pending_has_no_signature_or_event_projection() {
        let event = signed_event();
        let row = Row::from_parts(
            event.id,
            event.pubkey,
            event.created_at,
            event.kind,
            event.tags,
            event.content,
            RowSignature::Pending,
            BTreeSet::new(),
        );

        assert_eq!(row.signature(), RowSignature::Pending);
        assert!(row.signed_event().is_none());
    }

    #[test]
    fn signed_always_projects_the_exact_supplied_signature() {
        let event = signed_event();
        let expected = event.sig;
        let row = Row::from_parts(
            event.id,
            event.pubkey,
            event.created_at,
            event.kind,
            event.tags,
            event.content,
            RowSignature::Signed(expected),
            BTreeSet::new(),
        );

        assert_eq!(row.signature(), RowSignature::Signed(expected));
        assert_eq!(
            row.signed_event().expect("signed row has event").sig,
            expected
        );
    }
}

/// The canonical row is the ordinary reply/quote/reaction target, so it is
/// what `EventBuilder::tag` is usually handed.
///
/// A `Row` adds exactly ONE thing to the bare signed event `nmp-grammar`
/// already knows how to point at: the verified relay hint. Everything else —
/// the thread-position reading, the letter, the author slot, the companion
/// `p` row, the carried mentions and the dedup — is grammar's, delegated to
/// rather than restated, so a `Row` and a bare `nostr::Event` can never drift
/// into two dialects.
impl RootScope for Row {
    fn root_rows(&self, options: &TagOptions) -> Vec<nostr::Tag> {
        event_root_rows(&self.event_for_store(), self.verified_hint(), options)
    }

    fn parent_rows(&self, options: &TagOptions) -> Vec<nostr::Tag> {
        event_parent_rows(&self.event_for_store(), self.verified_hint(), options)
    }

    fn entity_kind(&self) -> Option<nostr::Kind> {
        Some(self.body.kind)
    }
}

/// A row-set delta (plan §7 non-goal: no ordering/windowing in M3 — raw
/// deltas + coverage only). This is the standard reactive-query contract:
/// `Effect::EmitRows` NEVER re-sends the query's full
/// current row set -- only the rows ADDED and REMOVED since that handle's
/// LAST emit (`refresh_observation`'s job). The FIRST emit for a fresh subscribe
/// is "every currently-matching row, as `Added`" (there is nothing to diff
/// against yet); an identity re-root (`set_active_pubkey`) that swaps the
/// whole row set falls out of the SAME diff -- "remove everything old, add
/// everything new" -- with no special-casing. Without this contract, a
/// long-running subscription that keeps matching new events re-delivers its
/// ENTIRE growing row set on every single ingest: O(rows) work per event,
/// O(rows²) total over a session (confirmed live: ~3.35M raw row deliveries
/// for ~2,587 distinct notes in 20s against real relays --
/// `docs/known-gaps.md`'s P0).
///
/// Runtime delivery may compose several of these reducer deltas into one
/// exact transition rebased onto the observer's last delivered batch (#46);
/// that preserves this incremental contract while bounding a slow observer's
/// pending backlog.
#[derive(Debug, Clone)]
pub enum RowDelta {
    /// A row that newly matches the query, carrying the full row (event +
    /// its current relay-provenance set) so the app never has to look
    /// either up separately.
    Added(Row),
    /// A row that already matches changed without changing event id. Carries
    /// the complete current row so signature promotion and simultaneous
    /// provenance growth compose without either fact being lost.
    Updated(Row),
    /// The SAME row already matched (#105): its relay-provenance SET grew --
    /// a relay not already in it delivered this exact event id. This is a
    /// `BTreeSet<RelayUrl>` compare, not a timestamp compare: an
    /// already-seen relay redelivering at a strictly later timestamp DOES
    /// advance `nmp_store::Provenance::merge_observation`'s internal
    /// watermark, but the projected SET is unchanged, so it correctly does
    /// NOT fire this variant (the "no spurious update for an identical
    /// observation" bar applies to the set, which is all this surface ever
    /// exposes). The event body itself is unchanged, so only the id and the
    /// row's FULL current source set are carried (matching `Added`'s own
    /// "whole value, not a patch" shape) -- never fired for a no-op
    /// redelivery, and never fired merely because SOME OTHER handle's
    /// lifecycle event forced a `refresh_observation` recompute of this one.
    SourcesGrew {
        id: EventId,
        sources: BTreeSet<RelayUrl>,
    },
    /// A row that no longer matches the query. Carries only the id -- the
    /// app is expected to already hold the event from an earlier `Added`
    /// (raw deltas + coverage only: no second copy of the payload is kept
    /// around just to hand back on removal).
    Removed(EventId),
}

impl RowDelta {
    /// The event id this delta concerns, regardless of variant.
    pub fn id(&self) -> EventId {
        match self {
            RowDelta::Added(row) => row.id(),
            RowDelta::Updated(row) => row.id(),
            RowDelta::SourcesGrew { id, .. } => *id,
            RowDelta::Removed(id) => *id,
        }
    }

    /// The complete row payload for `Added` and `Updated` deltas.
    pub fn row(&self) -> Option<&Row> {
        match self {
            RowDelta::Added(row) | RowDelta::Updated(row) => Some(row),
            RowDelta::SourcesGrew { .. } | RowDelta::Removed(_) => None,
        }
    }
}
