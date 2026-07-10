//! `nmp-store` — `EventStore` trait + `MemoryStore`: the one mutating door
//! (VISION §4 "the store", bug-class ledger #1).
//!
//! Insert runs **dedup-by-id first**, THEN replaceable/addressable
//! supersession (M1 plan §2.2): winner = newest `created_at`, tie-break
//! lexicographically-smallest id. `query` reuses `nostr::Filter::match_event`
//! — no hand-rolled event matching.
//!
//! Explicitly out of scope for M1 (deferred per plan §8): signature
//! verification, provenance (a `Duplicate` insert is a no-op stub), and GC.

mod address_key;
mod memory_store;

pub use memory_store::MemoryStore;

/// The result of an [`EventStore::insert`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    /// Brand-new event id, not part of any replaceable/addressable
    /// competition (or the first event at that address).
    Inserted,
    /// This exact event id is already present. Provenance merge is a no-op
    /// stub in M1 (no provenance field yet).
    Duplicate,
    /// A replaceable/addressable winner changed; `replaced` is the id of the
    /// event that is no longer the current winner for that address.
    Superseded {
        /// The event id that was superseded (dropped from the store).
        replaced: nostr::EventId,
    },
    /// This event is older than the current winner for its
    /// replaceable/addressable address (or ties on `created_at` but does not
    /// win the lexicographic id tie-break). Rejected: dropped, never stored.
    Stale,
}

/// The single mutating door onto the event store.
pub trait EventStore {
    /// Insert an event. Dedup-by-id first, then replaceable/addressable
    /// supersession.
    fn insert(&mut self, event: nostr::Event) -> InsertOutcome;

    /// Query current winners only (never a superseded/stale event), matched
    /// via `nostr::Filter::match_event`.
    fn query(&self, filter: &nostr::Filter) -> Vec<nostr::Event>;
}
