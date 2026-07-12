//! The scripted fake-relay / ingest harness (M1 plan §2.3) + event builders.
//! `Harness` wraps `Engine<MemoryStore>`; there is no network, no async —
//! `deliver` scripts what a real relay push would look like.
//!
//! The throwaway `Keys` used by the event builders below are test fixtures,
//! not a crypto feature — `MemoryStore` never verifies signatures (M1 plan
//! §8).

use std::collections::BTreeSet;

use nmp_grammar::{ConcreteFilter, ContextualAtom, DemandDelta};
use nmp_store::{
    sentinel_signature, AcceptOutcome, AcceptWrite, IntentSigState, MemoryStore, WriteDurability,
};
use nostr::{EventBuilder, Kind, Tag, Timestamp};

use crate::engine::{Engine, GraphSnapshot, HandleId, LiveQuery, Metrics, QueryHandle};

/// The scripted "fake relay" harness: `Engine<MemoryStore>` plus the
/// pass-through calls the contract tests drive.
pub struct Harness {
    engine: Engine<MemoryStore>,
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

impl Harness {
    pub fn new() -> Self {
        Self {
            engine: Engine::new(MemoryStore::new()),
        }
    }

    pub fn set_active(&mut self, pk: Option<nostr::PublicKey>) -> DemandDelta {
        self.engine.set_active_pubkey(pk)
    }

    pub fn subscribe(&mut self, q: LiveQuery) -> (QueryHandle, DemandDelta) {
        self.engine.subscribe(q)
    }

    /// Withdraw a subscription. Not in the plan's illustrative `Harness`
    /// sketch (§2.3), but required by contract test 8
    /// (`identical_descriptors_share_graph`), which exercises explicit
    /// unsubscribe/refcount behavior — a thin pass-through, same as every
    /// other method here.
    pub fn unsubscribe(&mut self, id: HandleId) -> DemandDelta {
        self.engine.unsubscribe(id)
    }

    /// Flush any handles dropped since the last mutating call (M1 nit #2 /
    /// M2 plan §8.2), surfacing their withdrawal even with no other
    /// activity to piggyback the drain on.
    pub fn poll_pending_drops(&mut self) -> DemandDelta {
        self.engine.poll_pending_drops()
    }

    /// Script a "relay push": insert `events` into the store and let the
    /// engine react (M1 plan §3.3 — the real path).
    pub fn deliver(&mut self, events: Vec<nostr::Event>) -> DemandDelta {
        self.engine.ingest(events)
    }

    /// Script a LOCAL optimistic write: enter `accept` through the
    /// `EventStore::accept_write` door and let the engine react
    /// (`crashsafe-accepted-2-3-plan.md` §1.2, U2). The pass-through mirror
    /// of `deliver` for the write side; unwraps the persistence `Result`
    /// (a volatile `MemoryStore` never fails a door) and returns both the
    /// store outcome (so a test can assert the `Inserted`/`Superseded`/
    /// `Stale` classification) and the `DemandDelta`.
    pub fn accept(&mut self, accept: AcceptWrite) -> (AcceptOutcome, DemandDelta) {
        self.engine
            .accept_local(accept)
            .expect("accept_write persistence (MemoryStore never fails a door)")
    }

    /// The wire-facing demand set (selection-only, two-hash-domains): what
    /// existing M1/M2 contract tests already assert against, unchanged in
    /// shape by #106.
    pub fn demand(&self) -> BTreeSet<ConcreteFilter> {
        self.engine
            .active_demand()
            .into_iter()
            .map(|atom| atom.filter)
            .collect()
    }

    /// The context-aware demand set (#106): identity-domain atoms, source/
    /// access included -- for tests that need to assert on
    /// `SourceAuthority`/`AccessContext`, e.g. equal-context-only
    /// coalescing (Fable D).
    pub fn demand_with_context(&self) -> BTreeSet<ContextualAtom> {
        self.engine.active_demand()
    }

    pub fn metrics(&self) -> Metrics {
        self.engine.metrics().clone()
    }

    pub fn graph_snapshot(&self) -> GraphSnapshot {
        self.engine.graph_snapshot()
    }
}

// ---- event builders (M1 plan §2.3) -------------------------------------
//
// Kind literals here are expected and fine: these functions build fixture
// *events*, not resolver routing logic. The kill guard (test 10) scans
// `src/**` excluding this file precisely because a real relay/event
// producer legitimately deals in concrete kinds; only the resolver's
// event-to-node routing must stay kind-blind.

/// A kind:1 (text note) event. Genuinely missing from M1's builder set
/// (M1's contract tests never needed a plain content note); added for M2's
/// differential oracle (`nmp-router/tests/differential_oracle.rs`), which
/// needs real signed content events to populate its per-relay model store.
pub fn kind1(author: &nostr::Keys, content: &str, created_at: u64) -> nostr::Event {
    EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

/// A kind:3 (contact list) event: `follows` encoded as `p` tags.
///
/// `allow_self_tagging()` is required here: `EventBuilder` otherwise
/// silently strips any `p` tag matching the signer's own pubkey (nostr
/// 0.44's default, meant to prevent accidental self-tagging), which would
/// drop a self-follow from `follows` before the event is ever signed —
/// several contract tests deliberately include the active pubkey in its own
/// follow/mute/member list.
pub fn kind3(author: &nostr::Keys, follows: &[nostr::PublicKey], created_at: u64) -> nostr::Event {
    EventBuilder::new(Kind::ContactList, "")
        .tags(follows.iter().map(|pk| Tag::public_key(*pk)))
        .allow_self_tagging()
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

/// A kind:39002 (NIP-29 group members, M1's stand-in) event: `group_d` as
/// the `d` tag, `members` as `p` tags.
pub fn kind39002(
    author: &nostr::Keys,
    group_d: &str,
    members: &[nostr::PublicKey],
    created_at: u64,
) -> nostr::Event {
    let mut tags = vec![Tag::identifier(group_d)];
    tags.extend(members.iter().map(|pk| Tag::public_key(*pk)));
    EventBuilder::new(Kind::from(39_002u16), "")
        .tags(tags)
        .allow_self_tagging()
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

/// A kind:10000 (NIP-51 mute list) event: `muted` encoded as `p` tags.
pub fn kind10000_mutes(
    author: &nostr::Keys,
    muted: &[nostr::PublicKey],
    created_at: u64,
) -> nostr::Event {
    EventBuilder::new(Kind::MuteList, "")
        .tags(muted.iter().map(|pk| Tag::public_key(*pk)))
        .allow_self_tagging()
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

/// A generic addressable event (`kind` in `30000..=39999` by NIP-01
/// convention, though this builder does not enforce that range) with a `d`
/// identifier tag.
pub fn addressable(author: &nostr::Keys, kind: u16, d: &str, created_at: u64) -> nostr::Event {
    EventBuilder::new(Kind::from(kind), "")
        .tag(Tag::identifier(d))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

/// A kind:10003 (NIP-51 bookmark list) event: `bookmarked` encoded as `e`
/// tags. Used only by contract test 12 (a third, unrelated depth-1 shape) —
/// not part of the plan's original builder list, added because the test
/// needs a fixture the existing four don't cover.
pub fn kind10003_bookmarks(
    author: &nostr::Keys,
    bookmarked: &[nostr::EventId],
    created_at: u64,
) -> nostr::Event {
    EventBuilder::new(Kind::from(10_003u16), "")
        .tags(bookmarked.iter().map(|id| Tag::event(*id)))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

/// A kind:5 (NIP-09 deletion) event `e`-tagging every id in `targets`.
/// Mirrors `nmp-store/tests/store_contract.rs`'s own `deletion_event`
/// fixture — needed here for #34's retraction-seam contract tests
/// (`docs/design/retraction-and-negative-deltas.md` §1.2/§2), which drive
/// `MemoryStore`'s real kind:5 processing through `Harness::deliver` rather
/// than mocking `InsertOutcome::Kind5Processed` directly.
pub fn deletion(author: &nostr::Keys, targets: &[nostr::EventId], created_at: u64) -> nostr::Event {
    EventBuilder::new(Kind::EventDeletion, "")
        .tags(targets.iter().map(|id| Tag::event(*id)))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

/// A kind:1 (text note) carrying a NIP-40 `expiration` tag. Mirrors
/// `nmp-store/tests/store_contract.rs`'s own `expiring_event` fixture —
/// needed here for #34's expiry-retraction contract tests
/// (`docs/design/retraction-and-negative-deltas.md` §3), which drive the
/// engine's real `store.expire_due`/`resolver.retract` path through a
/// synthetic-clock `tick` rather than mocking the removed row directly.
pub fn expiring_kind1(
    author: &nostr::Keys,
    content: &str,
    created_at: u64,
    expiration: u64,
) -> nostr::Event {
    EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from(created_at))
        .tag(Tag::expiration(Timestamp::from(expiration)))
        .sign_with_keys(author)
        .expect("test fixture event must sign cleanly")
}

/// Freeze any signed fixture event (`kind1`/`kind3`/`addressable`/… above)
/// into the sentinel-sig `AcceptWrite` the local write door takes — the
/// NIP-01 id never depends on `sig`, so the frozen body keeps the exact id
/// and matches queries identically to its eventual signed form
/// (`crashsafe-accepted-2-3-plan.md` §1.1 Q1). Mirrors
/// `nmp-store/tests/outbox_contract.rs`'s `compose`+`accept` fixtures so the
/// resolver's local-add contract exercises the SAME door shape. `accepted_at`
/// is the journal timestamp; the frozen body keeps its own `created_at`.
pub fn accept_write_of(signed: nostr::Event, accepted_at: u64) -> AcceptWrite {
    let frozen = nostr::Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        sentinel_signature(),
    );
    AcceptWrite {
        expected_pubkey: signed.pubkey,
        frozen,
        signing_identity_ref: "local".to_string(),
        durability: WriteDurability::Durable,
        routing: "author-outbox".to_string(),
        sig_state: IntentSigState::Pending,
        accepted_at: Timestamp::from(accepted_at),
    }
}
