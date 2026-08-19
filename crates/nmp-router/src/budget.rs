//! [`CompileBudget`] — every bound `Router::compile` plans WITHIN, in one
//! carrier (#931).
//!
//! Two bounds of different kinds travel together here, and keeping them
//! distinct is the point:
//!
//! The whole-demand RELAY CEILING is operator POLICY. It bounds how many
//! distinct relay sessions one compile may assemble, it is chosen by whoever
//! configured the engine, and nothing a relay says can move it.
//!
//! A per-relay SUBSCRIPTION BUDGET is a FACT the relay published about
//! itself: NIP-11 `limitation.max_subscriptions`, the number of concurrent
//! subscriptions it will hold open on one connection. Measured from live
//! public relays on 2026-07-27: 200 at relay.damus.io, 50 at nostr.wine and
//! purplepag.es, 20 at nos.lol, relay.primal.net and offchain.pub — and
//! nothing at all from relay.nostr.band or relay.snort.social, which serve no
//! NIP-11 document.
//!
//! **Absence is not a number.** An unadvertised relay is UNBUDGETED, and the
//! whole of the fail-open ruling is in that sentence: a fabricated default
//! would drop demand on a relay that never claimed a limit, and would flap
//! damus between 200 and a guess every time one HTTP GET failed. What guards
//! an unadvertised relay instead is the per-session subscription COUNT,
//! observable in [`crate::Diagnostics`] and asserted in the acceptance suite
//! — a fan-out escape is a defect for CI to catch, not a reason to refuse a
//! user's demand in production.
//!
//! Nothing here may ever feed identity. Wire ids are allocated tokens
//! ([`crate::SubId::allocate`]); NIP-11 documents refresh, and a mutable
//! derivation input is identity instability
//! (`docs/internals/subscriptions/identity-grouping-and-limits.md` §6).
//! `max_subid_length` in particular is carried for DIAGNOSIS only.

use std::collections::BTreeMap;

use crate::facts::RelayUrl;

/// The character length of every wire subscription id NMP sends: a
/// [`crate::SubId`]'s 256-bit digest in hex, exactly NIP-01's
/// `subscription_id` cap, never prefixed or truncated.
///
/// A relay advertising `max_subid_length` BELOW this rejects every REQ we
/// send it. That is a diagnosis, not a knob: shortening ids to fit would
/// mean deriving identity from a document that refreshes.
pub const WIRE_SUB_ID_CHARS: usize = 64;

/// What one relay ADVERTISED about itself, projected from NIP-11
/// `limitation`. `None` on a field means the relay said nothing about it —
/// never an implicit zero, never an implicit default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdvertisedRelayLimits {
    /// `limitation.max_subscriptions` — concurrent subscriptions this relay
    /// will hold open. ENFORCED when present.
    pub max_subscriptions: Option<usize>,
    /// `limitation.max_subid_length` — longest subscription id this relay
    /// accepts. DIAGNOSED when present, never enforced and never fed into id
    /// derivation.
    pub max_subid_length: Option<usize>,
}

/// The bounds one `Router::compile` plans within.
///
/// A bare `usize` converts into this ([`From<usize>`]) as "this relay
/// ceiling, and no relay has advertised anything" — so every caller that
/// only ever had a cap keeps saying exactly what it always said.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompileBudget {
    relay_cap: usize,
    advertised: BTreeMap<RelayUrl, AdvertisedRelayLimits>,
}

impl CompileBudget {
    /// The whole-demand relay ceiling alone — no relay has advertised
    /// anything, so no session is subscription-budgeted.
    #[must_use]
    pub fn with_relay_cap(relay_cap: usize) -> Self {
        Self {
            relay_cap,
            advertised: BTreeMap::new(),
        }
    }

    /// Record what `relay` published about itself. Builder-shaped so a test
    /// can state one relay's document in one expression; the engine builds
    /// the whole map from its retained NIP-11 evidence instead.
    #[must_use]
    pub fn advertising(mut self, relay: RelayUrl, limits: AdvertisedRelayLimits) -> Self {
        self.advertised.insert(relay, limits);
        self
    }

    /// Record every relay's advertisement at once.
    #[must_use]
    pub fn advertising_all(
        mut self,
        limits: impl IntoIterator<Item = (RelayUrl, AdvertisedRelayLimits)>,
    ) -> Self {
        self.advertised.extend(limits);
        self
    }

    /// The whole-demand relay ceiling.
    #[must_use]
    pub fn relay_cap(&self) -> usize {
        self.relay_cap
    }

    /// `relay`'s concurrent-subscription budget, or `None` when it
    /// advertised none — which means UNBUDGETED, never zero and never a
    /// default.
    #[must_use]
    pub fn max_subscriptions(&self, relay: &RelayUrl) -> Option<usize> {
        self.advertised
            .get(relay)
            .and_then(|limits| limits.max_subscriptions)
    }

    /// The longest subscription id `relay` says it accepts.
    #[must_use]
    pub fn max_subid_length(&self, relay: &RelayUrl) -> Option<usize> {
        self.advertised
            .get(relay)
            .and_then(|limits| limits.max_subid_length)
    }

    /// True iff `relay` advertised a subscription-id length SHORTER than the
    /// ids NMP sends, i.e. it would reject every REQ. Diagnostic only.
    #[must_use]
    pub fn rejects_our_subscription_ids(&self, relay: &RelayUrl) -> bool {
        self.max_subid_length(relay)
            .is_some_and(|length| length < WIRE_SUB_ID_CHARS)
    }
}

impl From<usize> for CompileBudget {
    fn from(relay_cap: usize) -> Self {
        Self::with_relay_cap(relay_cap)
    }
}

