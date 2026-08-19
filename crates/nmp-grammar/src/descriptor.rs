//! [`Demand`] — the full live-query identity (#106,
//! `docs/design/query-demand-and-evidence.md`): `selection + routing +
//! authenticated identity`, not filter-only. Two queries with the same [`Filter`] but
//! different intended routing must never collapse to the same atom/
//! refcount/coverage/attribution identity — that collapse (guarantee
//! #18) is exactly what conflating "what rows match" with "where reads come
//! from" caused.
//!
//! [`ReadRouting`] is a CLOSED vocabulary (VISION P4-style): extend the enum,
//! never admit a free-form config string. Authenticated identity is NOT a
//! vocabulary — it is an optional public key, absent by default.

use crate::binding::Filter;

/// Where a [`Demand`]'s reads come from. A strategy, not a resolved relay
/// set: re-executed against whatever the engine knows at each moment.
///
/// The whole app-facing routing vocabulary is these two words
/// (`docs/internals/routing/auto-and-explicit.md`), matching
/// [`crate::WriteRouting`] exactly. An app that says nothing gets
/// [`ReadRouting::Auto`] — naming a routing value is what an app does to
/// OVERRIDE NMP, never what it must do to use NMP.
///
/// Not `Copy`: `Explicit`'s relay set makes that impossible.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ReadRouting {
    /// "Figure out where to read this from." NMP applies whatever routing
    /// rule fits the demand: author outboxes (NIP-65 outbound) for a
    /// selection that resolves authors, a group's host relays, a DM inbox,
    /// relay hints and prior provenance, then the operator's app and
    /// fallback lanes. Outbox is the typical case, not the definition.
    ///
    /// This is ONE total path, not a branch: a selection that resolves no
    /// authors is the degenerate case of the same path (nothing for the
    /// coverage solve to do, so the remaining rules carry the whole route),
    /// never a separate routing class. That totality is what keeps `Auto`
    /// from being the filter-shape inference it replaced.
    #[default]
    Auto,
    /// "Ask these relays and that is that." Never widened to outbox,
    /// directory, app, fallback or indexer relays, regardless of whether
    /// the selection is author-bearing.
    ///
    /// Validated nonempty and normalized (sorted, deduplicated) at
    /// construction by [`Demand::new`] — the derived `Ord`/`Hash` and the
    /// context digest ([`crate::fold_context`]) both read this `Vec` in its
    /// own order, so a caller who skipped normalization could otherwise
    /// mint two representations of one routing intent.
    ///
    /// Cache-read behavior over this relay set (Agnostic vs Strict) is a
    /// SIBLING axis (`Demand::cache`), never nested here — see
    /// [`CacheMode`]'s doc.
    Explicit(Vec<nostr::RelayUrl>),
}

/// The complete identity of one physical relay session: a URL plus the
/// identity that session is bound to.
///
/// `Some(key)` is a session that authenticates as `key`. `None` is the
/// ordinary connection, bound to nobody.
///
/// What `None` means, exactly: such a connection never authenticates.
/// `CoreState::on_auth_challenge` returns immediately for it, so a relay's
/// challenge is dropped rather than routed to the installed policy. That is
/// the rule, not a gap: an identity is DECLARED, never discovered. Routing an
/// unbound session's challenge to the current account would let a connection
/// acquire an identity mid-flight and credit the coverage key it was already
/// accumulating under `None` to that identity. An identity is supplied up
/// front — by a write, which knows the key it is publishing as, or by a read
/// that names one.
///
/// One websocket carries at most one NIP-42 identity, which is why this —
/// and not the URL alone — is the session key: two accounts publishing to
/// the same relay concurrently are genuinely two sockets (`nmp-engine`'s
/// `same_url_keeps_distinct_signing_identities_in_worker_demand`). NIP-42
/// visibility is connection-scoped, so a URL without the identity its socket
/// is bound to is never a sufficient key for planning, transport,
/// attribution, replay, or coverage.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelaySessionKey {
    pub relay: nostr::RelayUrl,
    pub authenticate_as: Option<nostr::PublicKey>,
}

impl RelaySessionKey {
    #[must_use]
    pub const fn new(
        relay: nostr::RelayUrl,
        authenticate_as: Option<nostr::PublicKey>,
    ) -> Self {
        Self {
            relay,
            authenticate_as,
        }
    }

    /// The ordinary connection, bound to no identity. This is what every
    /// read plans against; it is not a category an app selects.
    #[must_use]
    pub const fn unauthenticated(relay: nostr::RelayUrl) -> Self {
        Self::new(relay, None)
    }
}

/// The cache-provenance mode a [`Demand`] carries -- meaningful ONLY under
/// [`ReadRouting::Explicit`] (#107's Contract: "pinned cache policy is part
/// of source identity"); a no-op under [`ReadRouting::Auto`], since there is
/// no explicit relay set to intersect against. Deliberately NOT part of
/// `ContextualAtom`'s hashed identity (`Demand::hash`-equivalent) — it
/// governs the LOCAL row-projection read (`nmp-engine`'s
/// `rows_and_evidence_for`), never wire/coverage identity (atlas's
/// #106/#107 seam ruling: the two axes are orthogonal). Consumed per-handle,
/// off `QueryHandle::cache()` -- never per-graph-node, since two handles
/// may share the identical (cache-free-deduped) `AcquisitionKey` while
/// disagreeing on `cache` (the #107 Done-when: "Same-filter Agnostic and
/// Strict handles remain distinct even when wire work coalesces").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum CacheMode {
    /// Serve every matching cached row regardless of provenance.
    #[default]
    Agnostic,
    /// Serve only cached rows whose unioned provenance set intersects an
    /// explicit relay set (meaningless/no-op under [`ReadRouting::Auto`]
    /// — #107).
    Strict,
}

/// How one query handle uses existing coverage when deciding whether it
/// contributes remote acquisition work. This is a third orthogonal axis on
/// the existing live-query noun, beside [`ReadRouting`] and
/// [`CacheMode`]; it is not part of [`crate::ContextualAtom`] identity.
/// Equal handles may therefore share their graph, rows, wire subscription,
/// and coverage history while making independent freshness decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Freshness {
    /// Cache-then-live: contribute ordinary wire work for the handle's
    /// lifetime. This is the pre-existing behavior and the default.
    #[default]
    Live,
    /// Suppress wire work when every currently planned source has coverage
    /// through at least `seconds` before the handle's opening-time engine
    /// clock. If not satisfied at opening, degrade once to [`Self::Live`].
    /// Whole seconds are exact: Nostr timestamps and coverage watermarks do
    /// not carry subsecond precision.
    MaxAge { seconds: u64 },
    /// Serve the canonical cache projection without ever contributing wire
    /// work, regardless of coverage or row presence.
    CacheOnly,
}

/// The full live-query declaration. Its semantic identity is
/// `selection + routing + authenticated identity` (#106); `cache` and
/// `freshness` remain per-handle policy axes.
/// `selection` is pure `Filter` — no context field is ever added to `Filter`
/// itself, keeping the grammar's own encoding/hashing untouched; `routing`
/// and the resolved identity fold into identity one level up, at
/// [`crate::ContextualAtom`].
///
/// Every field defaults, so the ordinary declaration is the selection and
/// nothing else:
///
/// ```
/// # use nmp_grammar::{Demand, Filter, ReadRouting};
/// let demand = Demand {
///     selection: Filter::default(),
///     ..Demand::default()
/// };
/// assert_eq!(demand.routing, ReadRouting::Auto);
/// ```
///
/// An [`ReadRouting::Explicit`] demand goes through [`Demand::new`] instead,
/// which is what validates and normalizes the relay set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Demand {
    pub selection: Filter,
    /// Where this demand's reads come from. Defaults to
    /// [`ReadRouting::Auto`]: an app that says nothing gets NMP's routing.
    pub routing: ReadRouting,
    /// The identity this demand's reads authenticate as. `None` — the
    /// default, and the overwhelmingly ordinary case — reads on the
    /// connection bound to nobody, which today never authenticates (see
    /// [`RelaySessionKey`] for exactly what that means and for the issue
    /// that changes it). `Some(key)` pins the reads to a session that
    /// authenticates as `key`.
    ///
    /// Carried verbatim into [`crate::ContextualAtom`] and from there into
    /// the session key: nothing resolves it, and no current account is
    /// consulted anywhere on this path.
    ///
    /// A defaulted field, never a constructor argument — the common case says
    /// nothing (`docs/internals/writes/identity.md`).
    pub authenticate_as: Option<nostr::PublicKey>,
    /// Orthogonal to `routing`/`access` (see [`CacheMode`]'s doc) — a
    /// sibling field, deliberately excluded from `ContextualAtom`'s hashed
    /// identity.
    pub cache: CacheMode,
    /// Per-handle acquisition freshness. Deliberately excluded from atom,
    /// wire, and coverage identity; see [`Freshness`].
    pub freshness: Freshness,
}

/// The unconstructible `Demand` combinations: `Demand::new` refuses these at
/// construction rather than silently producing a `Demand` whose routing path
/// resolves nothing forever.
///
/// There is exactly one, because [`ReadRouting::Auto`] is total — it has no
/// precondition a selection can fail to meet, which is the whole point of it
/// being the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemandError {
    /// [`ReadRouting::Explicit`] declared with an empty relay set (#107
    /// Contract: "the explicit relay set must be nonempty") -- there is
    /// nothing for the wire to ask.
    ExplicitRequiresNonemptyRelaySet,
}

impl std::fmt::Display for DemandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DemandError::ExplicitRequiresNonemptyRelaySet => {
                write!(f, "ReadRouting::Explicit requires a nonempty relay set")
            }
        }
    }
}

impl std::error::Error for DemandError {}

impl Demand {
    /// The validating constructor, for the one combination the plain
    /// `Demand { selection, ..Demand::default() }` declaration does not
    /// cover -- [`ReadRouting::Explicit`], whose relay set must be validated
    /// and normalized. `authenticate_as` is a defaulted FIELD, not an
    /// argument here: taxing every caller to write `None` is exactly the
    /// ceremony `docs/internals/writes/identity.md` exists to delete.
    ///
    /// Normalizes an `Explicit` relay set on the way in: sorted and
    /// deduplicated, so one routing intent has exactly ONE representation.
    /// This matters beyond tidiness. `Demand`'s derived `Ord`/`Hash` and
    /// [`crate::fold_context`]'s digest both read the `Vec` in its own
    /// order, so they agree by construction for any order — but without
    /// normalization `Explicit([b, a])` and `Explicit([a, b])` would be two
    /// distinct atoms, two refcount entries and two wire subscriptions for
    /// one demand.
    pub fn new(selection: Filter, routing: ReadRouting) -> Result<Self, DemandError> {
        let routing = match routing {
            ReadRouting::Auto => ReadRouting::Auto,
            ReadRouting::Explicit(relays) if relays.is_empty() => {
                return Err(DemandError::ExplicitRequiresNonemptyRelaySet);
            }
            ReadRouting::Explicit(mut relays) => {
                relays.sort();
                relays.dedup();
                ReadRouting::Explicit(relays)
            }
        };
        Ok(Self {
            selection,
            routing,
            authenticate_as: None,
            cache: CacheMode::Agnostic,
            freshness: Freshness::Live,
        })
    }

    /// The ONE identity projection (#106, Fable's ratified shape): which
    /// fields participate in atom/wire/coverage identity
    /// (`ContextualAtom`) -- `cache` is deliberately excluded (see
    /// [`CacheMode`]'s doc), which is what makes #107's addition of that
    /// field a one-line, identity-neutral change.
    pub fn atom_context(&self) -> (ReadRouting, Option<nostr::PublicKey>) {
        (self.routing.clone(), self.authenticate_as)
    }
}

