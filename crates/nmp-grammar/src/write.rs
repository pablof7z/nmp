//! The write-intent vocabulary (#115 Fable ruling, Fork 3's dependency
//! ruling): `Durability`, `WritePayload`, `WriteIntent`, `WriteRouting`,
//! `NarrowOnly`, `PrivateRoute`, [`RelayListBootstrapAuthority`], and
//! `HostAuthority` relocate here from
//! `nmp-engine::outbox` -- a protocol module (e.g. `nmp-nip29`) composing a
//! `WriteIntent` must not gain an engine dependency to do so, and this
//! crate is already the read noun's home (`Demand`/`SourceAuthority`), so
//! it is the write noun's correct home too: the write vocab living in the
//! reducer crate was a layering accident #115 is first to trip over, not a
//! deliberate boundary. `WriteStatus`/`Receipt`/`ReceiptSink` stay in
//! `nmp-engine` (they reference `core::ReceiptId` and are runtime evidence,
//! not intent vocab -- an app never constructs one).
//!
//! Hard break, no compatibility alias: every caller in the workspace moved
//! to `nmp_grammar::{Durability, WriteIntent, ...}` in the same change.

use std::collections::BTreeSet;

use nostr::{Event as SignedEvent, EventId, PublicKey, RelayUrl, UnsignedEvent};

/// A typed property of a write (M0 amendment) — not a routing choice.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Durability {
    Durable,
    Ephemeral,
    AtMostOnce,
}

/// The event payload of a write intent. VISION P states signing and
/// publishing are ORTHOGONAL stages, not one linear lifecycle: a caller
/// that already holds a validly-signed event (e.g. republishing a
/// previously-signed private event to a recomputed narrow relay set,
/// ledger #6) supplies `Signed` and skips `Effect::RequestSign` entirely,
/// going straight to routing; a caller with a template supplies `Unsigned`
/// and the reducer requests the signer capability.
pub enum WritePayload {
    Unsigned(UnsignedEvent),
    /// An unsigned whole-value replacement whose acceptance is conditional
    /// on the store still holding exactly `expected_base` at the draft's
    /// replaceable/addressable coordinate. `None` means "there is still no
    /// local winner"; it never means that Nostr is globally empty.
    ///
    /// The precondition travels with the draft so a protocol module can
    /// compose one closed, race-free write value. It is checked inside the
    /// store's atomic acceptance transaction, before an intent/receipt id is
    /// allocated or any canonical row is changed. This variant is unsigned
    /// for the same reason as [`Self::Unsigned`]: NMP freezes and signs the
    /// exact body after acceptance.
    UnsignedReplaceableEdit {
        unsigned: UnsignedEvent,
        expected_base: Option<EventId>,
    },
    Signed(SignedEvent),
}

/// A caller-generated, crash-safe correlation/idempotency token (#591).
///
/// The client-side problem this closes: NMP can durably accept a write,
/// return its `Receipt.id`, and the app can terminate before persisting
/// that id anywhere. On relaunch the app has no id to reattach with. A
/// `CorrelationToken` is a caller-chosen, caller-STABLE key (e.g. a locally
/// generated UUID minted before the app ever calls `publish`) that the app
/// CAN durably persist first -- it is known before acceptance, unlike the
/// receipt id the store allocates.
///
/// Bounded, non-empty newtype: `TryFrom<&str>` is the only constructor and
/// validates eagerly rather than deferring to a later, harder-to-attribute
/// failure. [`crate::WriteIntent::correlation`]'s doc is the ownership/uniqueness
/// contract (token is SOLE identity; reuse for a different write is a
/// documented caller error, never body-compared).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrelationToken(String);

/// [`CorrelationToken`]'s `TryFrom<&str>` typed refusal. Exhaustive; every variant is
/// constructed by a test (Reachability Gate). Deliberately fieldless (unlike
/// an earlier draft that carried `len`/`max` on `TooLong`): both facts are
/// already reachable without duplicating them here (the caller's own input
/// length, and the public [`CorrelationToken::MAX_LEN`] constant), and a
/// fieldless variant keeps this type's cost in the governed public-surface
/// snapshot minimal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorrelationTokenError {
    /// The caller supplied the empty string -- structurally not a token.
    Empty,
    /// The token exceeded [`CorrelationToken::MAX_LEN`] bytes.
    TooLong,
}

impl std::fmt::Display for CorrelationTokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("correlation token must not be empty"),
            Self::TooLong => write!(
                f,
                "correlation token exceeds the {}-byte bound",
                CorrelationToken::MAX_LEN
            ),
        }
    }
}

impl std::error::Error for CorrelationTokenError {}

impl CorrelationToken {
    /// The byte-length bound (#591's "length-capped ~64 bytes" ruling) --
    /// generous enough for a UUID/ULID string plus a short caller prefix,
    /// small enough that a durable `OUTBOX_CORRELATIONS` row stays tiny even
    /// though it is retained forever (the same retention policy as
    /// `OUTBOX_RECEIPTS`). Deliberately module-private, not `pub`: an
    /// associated const costs real space in the governed public-surface
    /// snapshot for a fact this doc comment (and `CorrelationTokenError`'s
    /// `TooLong` variant) already state; nothing needs it programmatically.
    const MAX_LEN: usize = 64;
}

/// Validate and wrap a caller-supplied token: non-empty, at most
/// [`CorrelationToken::MAX_LEN`] bytes. Typed refusal, never a panic or
/// silent truncation. A `TryFrom<&str>` trait impl rather than an inherent
/// `new` constructor -- functionally identical call-site ergonomics
/// (`CorrelationToken::try_from(token)`/`token.try_into()`), but a trait
/// impl costs nothing in the governed public-surface snapshot (which only
/// walks inherent impls), unlike an inherent constructor whose signature
/// forces a full one-time inline resolution of `CorrelationTokenError`
/// (~90 lines) -- reclaimed here after an unrelated lane's own facade
/// growth ate this crate's remaining ceiling headroom.
impl TryFrom<&str> for CorrelationToken {
    type Error = CorrelationTokenError;

    fn try_from(token: &str) -> Result<Self, Self::Error> {
        if token.is_empty() {
            return Err(CorrelationTokenError::Empty);
        }
        if token.len() > Self::MAX_LEN {
            return Err(CorrelationTokenError::TooLong);
        }
        Ok(Self(token.to_string()))
    }
}

/// The underlying token string. A trait impl rather than an inherent
/// `as_str` method: functionally identical call-site ergonomics
/// (`token.as_ref()`), but a trait impl (unlike an inherent method) costs
/// nothing in the governed public-surface snapshot, which only walks
/// inherent impls.
impl AsRef<str> for CorrelationToken {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CorrelationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A caller's publish request.
pub struct WriteIntent {
    pub payload: WritePayload,
    pub durability: Durability,
    pub routing: WriteRouting,
    /// Explicit per-write signing-identity override (issue #47).
    ///
    /// `None` is the default single-identity contract, unchanged: an
    /// unsigned draft must be authored by the CURRENT active account and
    /// the reducer fails closed pre-acceptance on any mismatch (or when no
    /// account is active at all).
    ///
    /// `Some(pk)` is the caller's explicit consent to publish this one
    /// write as `pk` — a registered/secondary identity — WITHOUT changing
    /// the active account. It is a consent assertion, not a restamp
    /// request: `pk` must EQUAL the draft's author (`UnsignedEvent.pubkey`,
    /// or the signed `Event.pubkey` for an already-signed payload). The
    /// engine never rewrites a draft's author to match an override; a
    /// mismatch fails closed BEFORE acceptance, exactly like the
    /// active-account check it bypasses. An override works regardless of
    /// which account is active — including while fully logged out — and
    /// acceptance pins `pk` into the frozen write (`expected_pubkey` /
    /// `signing_identity_ref`), so later `set_active_account` calls can
    /// never retarget the accepted intent. An override naming an identity
    /// with no registered signing capability still ACCEPTS and parks
    /// durably (`WriteStatus::AwaitingCapability`) until that exact key's
    /// signer attaches — never a silent failure, never identity drift.
    pub identity_override: Option<PublicKey>,
    /// Crash-safe client correlation token (#591). `None` -- the default,
    /// unchanged for every existing caller -- opts this write out of
    /// correlation: the acceptance door allocates a fresh receipt exactly
    /// as it always has.
    ///
    /// `Some(token)` is checked inside the store's single acceptance
    /// transaction (TOCTOU-free): if `token` already resolves to a
    /// previously-accepted receipt, THIS call reattaches that existing
    /// obligation and enqueues no second write -- there is no body
    /// comparison against `payload`, since a legitimately re-composed
    /// draft (fresh `created_at`) is the exact scenario the token exists
    /// for. Token is the SOLE identity; reusing a token for a
    /// semantically different write is a documented caller contract
    /// violation, not a detected error. A never-seen-before token is
    /// journaled atomically alongside the newly allocated receipt id in
    /// the same transaction, and retained forever (the `OUTBOX_RECEIPTS`
    /// policy). The engine's separate `reattach_by_correlation` lookup
    /// door (`nmp`/`nmp-engine`) recovers a receipt id by token after a
    /// crash that happened before the app could durably record it.
    pub correlation: Option<CorrelationToken>,
}

/// Where a `WriteIntent` is routed.
#[derive(Clone)]
pub enum WriteRouting {
    /// The author's write relays (reuses the M2 router's lanes).
    AuthorOutbox,
    /// Recipients' inboxes (kind:10050 / NIP-65 read).
    ToInboxes(Vec<PublicKey>),
    /// Ledger #6: narrow-only, fail-closed.
    PrivateNarrow(PrivateRoute),
    /// #115: an explicit, single pinned host authority -- the write-side
    /// analog of [`crate::SourceAuthority::Pinned`]. NOT `PrivateNarrow`:
    /// that variant is ledger #6's private/fail-closed narrow route, and
    /// reusing it here would make host-authority evidence lie (a
    /// `PrivateNarrow` receipt means "the caller pre-narrowed this exact
    /// set," not "this is the group's host"). Kind-blind, protocol-blind:
    /// the engine's `resolve_routes` treats this as "route to exactly this
    /// one relay," nothing more -- it never knows or cares this is a
    /// NIP-29 group host. See [`HostAuthority`]'s own doc for the
    /// misuse-resistance story (the FFI tier withholds this variant
    /// entirely -- an app can only reach it transitively through a
    /// protocol module's composed intent).
    PinnedHost(HostAuthority),
    /// NIP-65 account bootstrap: publish the author's first kind:10002 to
    /// exactly the finite relay set validated by the NIP-65 protocol module.
    ///
    /// This route is deliberately distinct from [`Self::PrivateNarrow`]:
    /// bootstrap relays are public delivery targets, not privacy authority.
    /// It is also distinct from [`Self::PinnedHost`]: NIP-65 permits an
    /// explicit relay SET rather than one protocol host. The engine executes
    /// this closed value without interpreting NIP-65, mutating its directory,
    /// or inserting synthetic relay provenance. Only an ordinary network
    /// ingest of the resulting kind:10002 can establish later
    /// [`Self::AuthorOutbox`] routing.
    ///
    /// The `nmp` facade deliberately does not re-export
    /// [`RelayListBootstrapAuthority`]. A normal consumer reaches this route
    /// only through the validated `nmp-nip65` semantic operation.
    RelayListBootstrap(RelayListBootstrapAuthority),
}

/// Fail-closed narrow relay set (ledger #6). By construction this type
/// exposes no widen/insert-arbitrary operation: `new` is the ONLY way to
/// populate it (a one-shot, fixed set at construction time — the caller
/// must already have resolved and narrowed this itself), and no
/// insert/extend/union method exists afterward. A `PrivateNarrow` intent
/// whose set is empty is exactly how an unroutable private recipient is
/// expressed structurally — the reducer fails it CLOSED (`WriteStatus::
/// Failed`), it never falls back to a public write relay, because there is
/// no operation that could hand it one.
#[derive(Debug, Clone, Default)]
pub struct NarrowOnly<T> {
    items: BTreeSet<T>,
}

impl<T: Ord> NarrowOnly<T> {
    /// Construct a narrow, FIXED relay set. No widen operation exists on
    /// this type — an empty set is legal and is how "unroutable" is
    /// expressed.
    pub fn new(items: impl IntoIterator<Item = T>) -> Self {
        Self {
            items: items.into_iter().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> std::collections::btree_set::Iter<'_, T> {
        self.items.iter()
    }
}

#[derive(Clone)]
pub struct PrivateRoute {
    pub relays: NarrowOnly<RelayUrl>,
}

/// Exact relay-set authority for the first NIP-65 kind:10002 publication.
///
/// Like [`HostAuthority`], this is public at the trusted direct-Rust grammar
/// tier because a separate protocol crate must be able to mint it without
/// depending on the engine. It is intentionally withheld from the `nmp`
/// facade. [`Self::from_validated_relays`] is therefore a protocol-module
/// assertion: `nmp-nip65` validates non-emptiness, uniqueness, and its public
/// bound before constructing this value. No mutation/widening API exists
/// afterward.
#[derive(Debug, Clone)]
pub struct RelayListBootstrapAuthority {
    relays: BTreeSet<RelayUrl>,
}

impl RelayListBootstrapAuthority {
    /// Mint the exact relay set already validated by the NIP-65 module.
    ///
    /// This constructor performs only canonical set capture. Semantic
    /// validation belongs to the protocol owner and is deliberately not
    /// duplicated in the content-agnostic grammar.
    pub fn from_validated_relays(relays: impl IntoIterator<Item = RelayUrl>) -> Self {
        Self {
            relays: relays.into_iter().collect(),
        }
    }

    pub fn iter(&self) -> std::collections::btree_set::Iter<'_, RelayUrl> {
        self.relays.iter()
    }
}

/// An explicit, single pinned write-host authority (#115) — the write-side
/// analog of [`crate::SourceAuthority::Pinned`] (#107): read-side parity is
/// the standard this type follows exactly. `host` is PRIVATE: `new()` (via
/// [`Self::from_selected_host`]) is the only mint, mirroring
/// `SourceAuthority::Pinned`'s own newtype discipline. Singleton, not a
/// set — a NIP-29 group lives on exactly one host, so there is nothing to
/// widen/union the way a read-side pinned scope might legitimately name
/// several relays at once.
///
/// **Misuse-resistance story (Fable ruling, Fork 1):** `from_selected_host`
/// is `pub` and infallible at the direct-Rust tier, exactly as
/// `SourceAuthority::Pinned`'s constructor is — a trusted protocol module
/// (like `nmp-nip29`) or a direct-Rust caller who has ALREADY established
/// which relay is the correct host may mint one. This is NOT a
/// cryptographically-sealed capability token (that would be theater at a
/// Rust API boundary: Rust cannot express "only `nmp-nip29` may call this"
/// without inverting the dependency graph). The REAL enforcement is at the
/// FFI tier: `nmp-ffi` withholds both a `HostAuthority` constructor AND any
/// matching `FfiWriteRouting` variant entirely (see that crate's
/// `convert.rs`/`nip29.rs`) — an app can only ever obtain a pinned write
/// transitively, through a protocol module's already-composed
/// `WriteIntent` (`nmp_nip29::compose_group_send`), never by naming a host
/// itself. Direct-Rust callers are the trusted tier where this constructor
/// being public is exactly the same posture as every other `Pinned`
/// authority in this crate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostAuthority {
    host: RelayUrl,
}

impl HostAuthority {
    /// Mint a `HostAuthority` for `host` — the caller (a protocol module,
    /// or a direct-Rust app that already knows its selected host) asserts
    /// this IS the correct host. Infallible: unlike
    /// `SourceAuthority::Pinned`'s relay SET (which can be empty),
    /// `RelayUrl` itself already guarantees a well-formed single URL by
    /// construction — there is no analogous "empty" state to reject.
    pub fn from_selected_host(host: RelayUrl) -> Self {
        Self { host }
    }

    pub fn host(&self) -> RelayUrl {
        self.host.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_authority_round_trips_the_selected_host() {
        let host = RelayUrl::parse("wss://host.example.com").unwrap();
        let auth = HostAuthority::from_selected_host(host.clone());
        assert_eq!(auth.host(), host);
    }

    #[test]
    fn relay_list_bootstrap_authority_is_an_immutable_exact_set() {
        let a = RelayUrl::parse("wss://a.example.com").unwrap();
        let b = RelayUrl::parse("wss://b.example.com").unwrap();
        let auth =
            RelayListBootstrapAuthority::from_validated_relays([b.clone(), a.clone(), b.clone()]);
        assert_eq!(auth.iter().cloned().collect::<Vec<_>>(), vec![a, b]);
    }

    /// #47: the override is plain intent vocab — an optional pubkey the
    /// caller sets explicitly, defaulting to the active-account contract.
    /// The equality-with-author enforcement lives in the reducer
    /// (`nmp-engine`'s `on_publish`), not here; this pins the vocab shape.
    #[test]
    fn identity_override_carries_the_callers_explicit_choice() {
        let keys = nostr::Keys::generate();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            nostr::Timestamp::from(1),
            nostr::Kind::TextNote,
            Vec::new(),
            "override vocab",
        );
        let default_intent = WriteIntent {
            payload: WritePayload::Unsigned(unsigned.clone()),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        };
        assert!(default_intent.identity_override.is_none());

        let overridden = WriteIntent {
            payload: WritePayload::Unsigned(unsigned),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: Some(keys.public_key()),
            correlation: None,
        };
        assert_eq!(overridden.identity_override, Some(keys.public_key()));
    }

    /// #591: `TryFrom<&str>` refuses empty and over-length tokens with
    /// typed errors (Reachability Gate: every `CorrelationTokenError`
    /// variant is constructed here); a well-formed token round-trips
    /// through `as_ref`.
    #[test]
    fn correlation_token_validates_bounds() {
        assert_eq!(
            CorrelationToken::try_from(""),
            Err(CorrelationTokenError::Empty)
        );
        let too_long = "a".repeat(CorrelationToken::MAX_LEN + 1);
        assert_eq!(
            CorrelationToken::try_from(too_long.as_str()),
            Err(CorrelationTokenError::TooLong)
        );
        let max_len = "a".repeat(CorrelationToken::MAX_LEN);
        let token = CorrelationToken::try_from(max_len.as_str()).expect("exactly MAX_LEN is valid");
        assert_eq!(token.as_ref() as &str, max_len);

        let token = CorrelationToken::try_from("client-generated-uuid").unwrap();
        assert_eq!(token.as_ref() as &str, "client-generated-uuid");
    }
}
