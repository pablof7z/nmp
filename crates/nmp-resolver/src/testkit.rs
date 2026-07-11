//! The scripted fake-relay / ingest harness (M1 plan §2.3) + event builders.
//! `Harness` wraps `Engine<MemoryStore>`; there is no network, no async —
//! `deliver` scripts what a real relay push would look like.
//!
//! The throwaway `Keys` used by the event builders below are test fixtures,
//! not a crypto feature — `MemoryStore` never verifies signatures (M1 plan
//! §8).

use std::collections::BTreeSet;

use nmp_grammar::{ConcreteFilter, DemandDelta};
use nmp_store::MemoryStore;
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

    pub fn demand(&self) -> BTreeSet<ConcreteFilter> {
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
