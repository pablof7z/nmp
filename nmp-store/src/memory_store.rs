//! [`MemoryStore`] — the M1 in-memory `EventStore`.

use std::cmp::Ordering;
use std::collections::HashMap;

use nostr::filter::MatchEventOptions;
use nostr::{Event, EventId, Filter};

use crate::address_key::{address_key_for, AddressKey};
use crate::{EventStore, InsertOutcome};

/// An in-memory `EventStore`. Holds exactly the currently-reachable events:
/// every "regular" (non-replaceable, non-addressable) event ever inserted,
/// plus the current winner (only) for every replaceable/addressable
/// address. No persistence, no signature verification, no provenance, no
/// GC (all deferred per M1 plan §8).
#[derive(Debug, Default)]
pub struct MemoryStore {
    by_id: HashMap<EventId, Event>,
    addr_index: HashMap<AddressKey, EventId>,
}

impl MemoryStore {
    /// A new, empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

/// True iff `candidate` wins over `current` for the same
/// replaceable/addressable address: newest `created_at` wins; on a
/// `created_at` tie, the lexicographically-smallest id wins.
fn candidate_wins(candidate: &Event, current: &Event) -> bool {
    match candidate.created_at.cmp(&current.created_at) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => candidate.id < current.id,
    }
}

impl EventStore for MemoryStore {
    fn insert(&mut self, event: Event) -> InsertOutcome {
        // Dedup-by-id FIRST, before any supersession logic.
        if self.by_id.contains_key(&event.id) {
            return InsertOutcome::Duplicate;
        }

        match address_key_for(&event) {
            None => {
                // Regular event: no competition, always inserted.
                self.by_id.insert(event.id, event);
                InsertOutcome::Inserted
            }
            Some(key) => match self.addr_index.get(&key).copied() {
                None => {
                    // First event ever seen at this address.
                    let id = event.id;
                    self.by_id.insert(id, event);
                    self.addr_index.insert(key, id);
                    InsertOutcome::Inserted
                }
                Some(current_id) => {
                    let current = self
                        .by_id
                        .get(&current_id)
                        .expect("addr_index must always point at a stored event");

                    if candidate_wins(&event, current) {
                        let replaced = current_id;
                        let new_id = event.id;
                        self.by_id.remove(&replaced);
                        self.by_id.insert(new_id, event);
                        self.addr_index.insert(key, new_id);
                        InsertOutcome::Superseded { replaced }
                    } else {
                        // Older-for-existing-address: rejected, dropped.
                        InsertOutcome::Stale
                    }
                }
            },
        }
    }

    fn query(&self, filter: &Filter) -> Vec<Event> {
        // `by_id` holds exactly the current winners (regular events, plus
        // the one live event per replaceable/addressable address) — so
        // iterating it and matching is "current winners only" by
        // construction. Matching is delegated entirely to
        // `nostr::Filter::match_event`; no hand-rolled matching here.
        self.by_id
            .values()
            .filter(|event| filter.match_event(event, MatchEventOptions::new()))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    fn keys() -> Keys {
        Keys::generate()
    }

    fn kind3_event(keys: &Keys, created_at: u64) -> Event {
        EventBuilder::new(Kind::ContactList, "")
            .custom_created_at(nostr::Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    fn addressable_event(keys: &Keys, kind: u16, d: &str, created_at: u64) -> Event {
        EventBuilder::new(Kind::from(kind), "")
            .tag(Tag::identifier(d))
            .custom_created_at(nostr::Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    fn regular_event(keys: &Keys, content: &str) -> Event {
        EventBuilder::new(Kind::TextNote, content)
            .sign_with_keys(keys)
            .unwrap()
    }

    #[test]
    fn newest_created_at_wins_replaceable() {
        let mut store = MemoryStore::new();
        let k = keys();

        let old = kind3_event(&k, 100);
        let old_id = old.id;
        assert_eq!(store.insert(old), InsertOutcome::Inserted);

        let newer = kind3_event(&k, 200);
        let newer_id = newer.id;
        assert_eq!(
            store.insert(newer),
            InsertOutcome::Superseded { replaced: old_id }
        );

        let results = store.query(&Filter::new().kind(Kind::ContactList).author(k.public_key()));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, newer_id);
    }

    #[test]
    fn lexically_smallest_id_wins_on_created_at_tie() {
        let mut store = MemoryStore::new();
        let k = keys();

        // Build several same-timestamp kind:3 events (distinct ids via
        // distinct tag content) to guarantee we can exhibit both a losing
        // and a winning tie-break without depending on random luck.
        let mut candidates: Vec<Event> = (0..6)
            .map(|i| {
                EventBuilder::new(Kind::ContactList, "")
                    .tag(Tag::hashtag(format!("salt{i}")))
                    .custom_created_at(nostr::Timestamp::from(100u64))
                    .sign_with_keys(&k)
                    .unwrap()
            })
            .collect();
        candidates.sort_by_key(|e| e.id);

        let smallest = candidates[0].clone();
        let larger = candidates[1].clone();

        // Insert the larger-id one first: it becomes the winner.
        assert_eq!(store.insert(larger.clone()), InsertOutcome::Inserted);

        // Now insert the lexicographically smaller id at the same
        // created_at: it must win (Superseded), even though it is not
        // newer.
        assert_eq!(
            store.insert(smallest.clone()),
            InsertOutcome::Superseded {
                replaced: larger.id
            }
        );

        // And inserting an even-larger id at the same created_at now must
        // lose (Stale) against the current (smallest) winner.
        let third = candidates[2].clone();
        assert_eq!(store.insert(third), InsertOutcome::Stale);

        let results = store.query(&Filter::new().kind(Kind::ContactList).author(k.public_key()));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, smallest.id);
    }

    #[test]
    fn stale_older_event_rejected() {
        let mut store = MemoryStore::new();
        let k = keys();

        let newer = kind3_event(&k, 200);
        assert_eq!(store.insert(newer.clone()), InsertOutcome::Inserted);

        let older = kind3_event(&k, 100);
        assert_eq!(store.insert(older), InsertOutcome::Stale);

        let results = store.query(&Filter::new().kind(Kind::ContactList).author(k.public_key()));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, newer.id);
    }

    #[test]
    fn duplicate_id_delivery_is_a_noop() {
        let mut store = MemoryStore::new();
        let k = keys();
        let event = regular_event(&k, "hello");

        assert_eq!(store.insert(event.clone()), InsertOutcome::Inserted);
        assert_eq!(store.insert(event), InsertOutcome::Duplicate);

        let results = store.query(&Filter::new().kind(Kind::TextNote).author(k.public_key()));
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn replaceable_keyed_by_pubkey_kind_not_by_id_alone() {
        let mut store = MemoryStore::new();
        let alice = keys();
        let bob = keys();

        // Same kind, different authors: independent addresses, both live.
        assert_eq!(
            store.insert(kind3_event(&alice, 100)),
            InsertOutcome::Inserted
        );
        assert_eq!(
            store.insert(kind3_event(&bob, 100)),
            InsertOutcome::Inserted
        );

        let results = store.query(&Filter::new().kind(Kind::ContactList));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn addressable_keyed_by_pubkey_kind_d_distinct_from_replaceable() {
        let mut store = MemoryStore::new();
        let k = keys();

        let g1_old = addressable_event(&k, 30_003, "g1", 100);
        let g1_old_id = g1_old.id;
        assert_eq!(store.insert(g1_old), InsertOutcome::Inserted);

        // Different `d` under the same (pubkey, kind): independent address,
        // not competing with g1.
        let g2 = addressable_event(&k, 30_003, "g2", 100);
        assert_eq!(store.insert(g2.clone()), InsertOutcome::Inserted);

        // Newer g1 supersedes only g1, leaves g2 untouched.
        let g1_new = addressable_event(&k, 30_003, "g1", 200);
        let g1_new_id = g1_new.id;
        assert_eq!(
            store.insert(g1_new),
            InsertOutcome::Superseded {
                replaced: g1_old_id
            }
        );

        let mut results = store.query(
            &Filter::new()
                .kind(Kind::from(30_003u16))
                .author(k.public_key()),
        );
        results.sort_by_key(|e| e.id);
        let mut expected = vec![g1_new_id, g2.id];
        expected.sort();
        assert_eq!(results.iter().map(|e| e.id).collect::<Vec<_>>(), expected);
    }

    #[test]
    fn query_returns_only_current_winners_never_superseded() {
        let mut store = MemoryStore::new();
        let k = keys();

        let old = kind3_event(&k, 100);
        let old_id = old.id;
        store.insert(old);
        let newer = kind3_event(&k, 200);
        store.insert(newer);

        let results = store.query(&Filter::new()); // unconstrained: match everything
        assert!(!results.iter().any(|e| e.id == old_id));
    }
}
