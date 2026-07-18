//! Bounded, volatile eligibility for exact already-committed relay observations.
//!
//! This is an optimization cache, never event authority. A miss, eviction,
//! poisoned lock, or restart always falls back to the ordinary parse/verify/
//! governed-ingest path. The engine is the only publisher and does so only
//! after its store transaction commits.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use nostr::{EventId, RelayUrl};
use tungstenite::Utf8Bytes;

pub(crate) type EventDigest = [u8; 32];

/// Allocation-free cache scope derived from the already-canonical relay URL.
/// The cache already relies on BLAKE3 collision resistance for exact EVENT
/// identity; using the same full-width digest for its relay half avoids
/// cloning and hashing `url::Url` on every replay observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct RelayScope([u8; 32]);

impl RelayScope {
    pub(super) fn new(relay: &RelayUrl) -> Self {
        Self(*blake3::hash(relay.as_str().as_bytes()).as_bytes())
    }
}

/// Preparse identity carried beside an ordinary verified EVENT until its
/// governed transaction decides whether that exact observation is current.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommittedObservationCandidate {
    digest: EventDigest,
}

impl CommittedObservationCandidate {
    pub(crate) const fn new(digest: EventDigest) -> Self {
        Self { digest }
    }

    #[must_use]
    pub const fn digest(&self) -> EventDigest {
        self.digest
    }
}

/// One engine-authorized post-commit cache publication.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedObservationPublication {
    relay: RelayScope,
    digest: EventDigest,
    event_id: EventId,
    event_kind: u16,
}

impl CommittedObservationPublication {
    #[must_use]
    pub fn new(
        relay: RelayUrl,
        candidate: CommittedObservationCandidate,
        event_id: EventId,
        event_kind: u16,
    ) -> Self {
        Self {
            relay: RelayScope::new(&relay),
            digest: candidate.digest,
            event_id,
            event_kind,
        }
    }
}

/// A preparse hit keeps the websocket-owned text so engine-side epoch or
/// pending-intent rejection can recover the exact ordinary path.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedObservationHit {
    raw_text: Utf8Bytes,
    relay: RelayScope,
    digest: EventDigest,
    slot: usize,
    epoch: u64,
    event_id: EventId,
    event_kind: u16,
}

impl CommittedObservationHit {
    #[must_use]
    pub const fn event_id(&self) -> EventId {
        self.event_id
    }

    #[must_use]
    pub const fn event_kind(&self) -> u16 {
        self.event_kind
    }

    #[must_use]
    pub fn encoded_bytes(&self) -> usize {
        self.raw_text.len()
    }

    pub(crate) fn raw_text(&self) -> &str {
        self.raw_text.as_str()
    }

    pub(crate) fn into_raw_and_candidate(self) -> (Utf8Bytes, CommittedObservationCandidate) {
        (
            self.raw_text,
            CommittedObservationCandidate::new(self.digest),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    relay: RelayScope,
    digest: EventDigest,
}

#[derive(Debug)]
struct Slot {
    key: CacheKey,
    epoch: u64,
    event_id: EventId,
    event_kind: u16,
    event_prev: Option<usize>,
    event_next: Option<usize>,
}

#[derive(Debug)]
struct CacheInner {
    capacity: usize,
    entries: HashMap<CacheKey, usize>,
    slots: Vec<Option<Slot>>,
    free: Vec<usize>,
    insertion_order: VecDeque<(usize, u64)>,
    /// Intrusive per-event slot chains avoid one heap allocation per cached
    /// EventId while retaining O(number of relay observations) invalidation.
    by_event: HashMap<EventId, usize>,
    next_epoch: u64,
}

impl CacheInner {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::with_capacity(capacity),
            slots: Vec::with_capacity(capacity),
            free: Vec::new(),
            insertion_order: VecDeque::with_capacity(capacity),
            by_event: HashMap::new(),
            next_epoch: 1,
        }
    }

    fn mint_epoch(&mut self) -> u64 {
        if self.next_epoch == u64::MAX {
            self.clear();
        }
        let epoch = self.next_epoch;
        self.next_epoch += 1;
        epoch
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.slots.clear();
        self.free.clear();
        self.insertion_order.clear();
        self.by_event.clear();
        self.next_epoch = 1;
    }

    fn remove_slot(&mut self, index: usize) -> bool {
        let Some(slot) = self.slots.get_mut(index).and_then(Option::take) else {
            return false;
        };
        self.entries.remove(&slot.key);
        if let Some(previous) = slot.event_prev {
            if let Some(previous) = self.slots.get_mut(previous).and_then(Option::as_mut) {
                previous.event_next = slot.event_next;
            }
        } else {
            match slot.event_next {
                Some(next) => {
                    self.by_event.insert(slot.event_id, next);
                }
                None => {
                    self.by_event.remove(&slot.event_id);
                }
            }
        }
        if let Some(next) = slot.event_next {
            if let Some(next) = self.slots.get_mut(next).and_then(Option::as_mut) {
                next.event_prev = slot.event_prev;
            }
        }
        self.free.push(index);
        true
    }

    fn evict_one(&mut self) {
        while let Some((index, epoch)) = self.insertion_order.pop_front() {
            if self
                .slots
                .get(index)
                .and_then(Option::as_ref)
                .is_some_and(|slot| slot.epoch == epoch)
            {
                self.remove_slot(index);
                return;
            }
        }
    }

    fn publish(&mut self, publication: CommittedObservationPublication) -> bool {
        if self.capacity == 0 {
            return false;
        }
        let key = CacheKey {
            relay: publication.relay,
            digest: publication.digest,
        };
        if let Some(index) = self.entries.get(&key).copied() {
            let Some(existing) = self.slots.get(index).and_then(Option::as_ref) else {
                self.entries.remove(&key);
                return false;
            };
            if existing.event_id == publication.event_id
                && existing.event_kind == publication.event_kind
            {
                return false;
            }
            // A digest/key collision must never authorize the existing bytes.
            // Remove both identities and let future observations use the exact
            // ordinary path instead of replacing one with the other.
            self.remove_slot(index);
            return false;
        }
        if self.entries.len() == self.capacity {
            self.evict_one();
        }
        let epoch = self.mint_epoch();
        let index = self.free.pop().unwrap_or_else(|| {
            let index = self.slots.len();
            self.slots.push(None);
            index
        });
        let event_id = publication.event_id;
        let event_next = self.by_event.insert(event_id, index);
        if let Some(next) = event_next {
            if let Some(next) = self.slots.get_mut(next).and_then(Option::as_mut) {
                next.event_prev = Some(index);
            }
        }
        self.slots[index] = Some(Slot {
            key: key.clone(),
            epoch,
            event_id,
            event_kind: publication.event_kind,
            event_prev: None,
            event_next,
        });
        self.entries.insert(key, index);
        self.insertion_order.push_back((index, epoch));
        true
    }
}

#[derive(Debug)]
pub(super) struct CommittedObservationCache {
    inner: Mutex<CacheInner>,
}

impl CommittedObservationCache {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(CacheInner::new(capacity)),
        }
    }

    pub(super) fn lookup(
        &self,
        relay: RelayScope,
        digest: EventDigest,
        raw_text: Utf8Bytes,
    ) -> Result<CommittedObservationHit, Utf8Bytes> {
        let Ok(inner) = self.inner.lock() else {
            return Err(raw_text);
        };
        let key = CacheKey { relay, digest };
        let Some(index) = inner.entries.get(&key).copied() else {
            #[cfg(feature = "bench-instrumentation")]
            crate::ingest_attribution::committed_observation_lookup(false);
            return Err(raw_text);
        };
        let Some(slot) = inner.slots.get(index).and_then(Option::as_ref) else {
            #[cfg(feature = "bench-instrumentation")]
            crate::ingest_attribution::committed_observation_lookup(false);
            return Err(raw_text);
        };
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::committed_observation_lookup(true);
        Ok(CommittedObservationHit {
            raw_text,
            relay,
            digest,
            slot: index,
            epoch: slot.epoch,
            event_id: slot.event_id,
            event_kind: slot.event_kind,
        })
    }

    pub(super) fn revalidate_all<'a>(
        &self,
        hits: impl IntoIterator<Item = &'a CommittedObservationHit>,
    ) -> bool {
        let Ok(inner) = self.inner.lock() else {
            return false;
        };
        hits.into_iter().all(|hit| {
            inner
                .slots
                .get(hit.slot)
                .and_then(Option::as_ref)
                .is_some_and(|slot| {
                    slot.epoch == hit.epoch
                        && slot.event_id == hit.event_id
                        && slot.event_kind == hit.event_kind
                        && slot.key.relay == hit.relay
                        && slot.key.digest == hit.digest
                })
        })
    }

    pub(super) fn apply_update(
        &self,
        invalidated: impl IntoIterator<Item = EventId>,
        published: impl IntoIterator<Item = CommittedObservationPublication>,
    ) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        #[cfg(feature = "bench-instrumentation")]
        let mut invalidation_count = 0_u64;
        for event_id in invalidated {
            while let Some(index) = inner.by_event.get(&event_id).copied() {
                let _removed = inner.remove_slot(index);
                #[cfg(feature = "bench-instrumentation")]
                {
                    invalidation_count += _removed as u64;
                }
            }
        }
        #[cfg(feature = "bench-instrumentation")]
        let mut publication_count = 0_u64;
        for publication in published {
            let _inserted = inner.publish(publication);
            #[cfg(feature = "bench-instrumentation")]
            {
                publication_count += _inserted as u64;
            }
        }
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::committed_observation_update(
            publication_count,
            invalidation_count,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay(value: &str) -> RelayUrl {
        RelayUrl::parse(value).unwrap()
    }

    fn scope(relay: &RelayUrl) -> RelayScope {
        RelayScope::new(relay)
    }

    #[test]
    fn eviction_and_invalidation_make_old_leases_fail_closed() {
        let cache = CommittedObservationCache::new(1);
        let first_id = EventId::from_byte_array([1; 32]);
        let second_id = EventId::from_byte_array([2; 32]);
        let relay = relay("wss://relay.example");
        cache.apply_update(
            [],
            [CommittedObservationPublication::new(
                relay.clone(),
                CommittedObservationCandidate::new([3; 32]),
                first_id,
                1,
            )],
        );
        let first = cache
            .lookup(scope(&relay), [3; 32], Utf8Bytes::from_static("first"))
            .unwrap();
        cache.apply_update(
            [],
            [CommittedObservationPublication::new(
                relay.clone(),
                CommittedObservationCandidate::new([4; 32]),
                second_id,
                2,
            )],
        );
        assert!(!cache.revalidate_all([&first]));

        let second = cache
            .lookup(scope(&relay), [4; 32], Utf8Bytes::from_static("second"))
            .unwrap();
        cache.apply_update([second_id], []);
        assert!(!cache.revalidate_all([&second]));
    }

    #[test]
    fn another_relay_and_digest_are_distinct() {
        let cache = CommittedObservationCache::new(2);
        let first = relay("wss://one.example");
        let second = relay("wss://two.example");
        cache.apply_update(
            [],
            [CommittedObservationPublication::new(
                first.clone(),
                CommittedObservationCandidate::new([7; 32]),
                EventId::from_byte_array([8; 32]),
                1,
            )],
        );
        assert!(cache
            .lookup(scope(&second), [7; 32], Utf8Bytes::from_static("raw"))
            .is_err());
        assert!(cache
            .lookup(scope(&first), [9; 32], Utf8Bytes::from_static("raw"))
            .is_err());
    }

    #[test]
    fn event_invalidation_removes_every_relay_after_partial_eviction() {
        let cache = CommittedObservationCache::new(2);
        let event_id = EventId::from_byte_array([10; 32]);
        let first = relay("wss://first.example");
        let second = relay("wss://second.example");
        cache.apply_update(
            [],
            [
                CommittedObservationPublication::new(
                    first.clone(),
                    CommittedObservationCandidate::new([11; 32]),
                    event_id,
                    1,
                ),
                CommittedObservationPublication::new(
                    second.clone(),
                    CommittedObservationCandidate::new([12; 32]),
                    event_id,
                    1,
                ),
            ],
        );
        cache.apply_update(
            [],
            [CommittedObservationPublication::new(
                first.clone(),
                CommittedObservationCandidate::new([13; 32]),
                EventId::from_byte_array([14; 32]),
                2,
            )],
        );
        assert!(cache
            .lookup(scope(&first), [11; 32], Utf8Bytes::from_static("evicted"))
            .is_err());
        assert!(cache
            .lookup(
                scope(&second),
                [12; 32],
                Utf8Bytes::from_static("remaining"),
            )
            .is_ok());

        cache.apply_update([event_id], []);
        assert!(cache
            .lookup(
                scope(&second),
                [12; 32],
                Utf8Bytes::from_static("invalidated"),
            )
            .is_err());
    }
}
