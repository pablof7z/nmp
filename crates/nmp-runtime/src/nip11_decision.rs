//! The NIP-11 grace-fallback deadline (#1731). Moved out of `lib.rs` beside
//! its sibling owners — see `identity_sessions`'s module doc for why this is
//! a module and not a crate.
//!
//! This is the deadline state machine that decides WHEN a fallback fires.
//! `nip11.rs` is a different concern: the value projection between a fetched
//! document and the reducer's evidence type.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use nostr::RelayUrl;

/// NIP-11 may refine a capability decision, but a slow/unavailable HTTP
/// endpoint must not hold the WebSocket protocol path hostage. This is a
/// one-shot grace window, not polling; the eventual document still updates
/// diagnostics/cache after the behavioral probe has begun.
const NIP11_DECISION_GRACE: Duration = Duration::from_millis(250);

#[derive(Default)]
pub(super) struct Nip11DecisionState {
    next_generation: u64,
    pending: HashMap<RelayUrl, Nip11Decision>,
}

struct Nip11Decision {
    generation: u64,
    deadline: Instant,
    fallback_sent: bool,
}

impl Nip11DecisionState {
    pub(super) fn begin(&mut self, url: RelayUrl, now: Instant) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.pending.insert(
            url,
            Nip11Decision {
                generation,
                deadline: now + NIP11_DECISION_GRACE,
                fallback_sent: false,
            },
        );
        generation
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .values()
            .filter(|decision| !decision.fallback_sent)
            .map(|decision| decision.deadline)
            .min()
    }

    pub(super) fn take_due_fallbacks(&mut self, now: Instant) -> Vec<RelayUrl> {
        let mut due = Vec::new();
        for (url, decision) in &mut self.pending {
            if !decision.fallback_sent && decision.deadline <= now {
                decision.fallback_sent = true;
                due.push(url.clone());
            }
        }
        due
    }

    pub(super) fn complete(&mut self, url: &RelayUrl, generation: u64) -> bool {
        if !self
            .pending
            .get(url)
            .is_some_and(|decision| decision.generation == generation)
        {
            return false;
        }
        self.pending.remove(url);
        true
    }

    pub(super) fn refuse(&mut self, url: &RelayUrl, generation: u64) {
        if self
            .pending
            .get(url)
            .is_some_and(|decision| decision.generation == generation)
        {
            self.pending.remove(url);
        }
    }
}

#[cfg(test)]
mod nip11_decision_tests {
    use super::*;

    #[test]
    fn grace_fallback_is_independent_and_eventual_completion_is_generation_guarded() {
        let relay = RelayUrl::parse("wss://decision.example").unwrap();
        let now = Instant::now();
        let mut state = Nip11DecisionState::default();
        let generation = state.begin(relay.clone(), now);

        assert!(state
            .take_due_fallbacks(now + NIP11_DECISION_GRACE - Duration::from_millis(1))
            .is_empty());
        assert_eq!(
            state.take_due_fallbacks(now + NIP11_DECISION_GRACE),
            vec![relay.clone()]
        );
        assert!(state
            .take_due_fallbacks(now + NIP11_DECISION_GRACE + Duration::from_secs(1))
            .is_empty());
        assert!(!state.complete(&relay, generation.wrapping_add(1)));
        assert!(state.complete(&relay, generation));
        assert!(state.pending.is_empty());
    }
}
