//! The write-intent vocabulary (#115 Fable ruling, Fork 3's dependency
//! ruling): `EventBuilder`, `WritePayload`, `WriteIntent`, and
//! `WriteRouting` live here rather than `nmp-engine::outbox`: protocol modules composing a
//! `WriteIntent` must not gain an engine dependency to do so, and this crate
//! is already the read noun's home (`Demand`/`SourceAuthority`).
//! `WriteFact` and `Receipt` stay in `nmp` because they are runtime
//! evidence rather than intent vocabulary; live delivery capabilities are
//! runtime-private and never enter the reducer.
//!
//! Hard break, no compatibility alias: every caller in the workspace moved
//! to `nmp_grammar::{WriteIntent, ...}` in the same change.

use nostr::{
    Event as SignedEvent, EventId, Kind, PublicKey, RelayUrl, Tag, Timestamp, UnsignedEvent,
};

/// Everything an app must say to publish an event, and everything it MAY
/// say. The kind is the one thing NMP cannot invent, so the kind is the one
/// thing this type demands; `created_at`, the author, the id and the
/// signature are filled in when the app did not say them.
///
/// **It is a value, not an object**, and that is load-bearing. It carries no
/// engine reference, no session and no signer handle: composing one is pure
/// and infallible, and everything that can fail — no active account, no
/// registered signer, a stale replaceable base — fails at the one publish
/// door. More importantly it **structurally cannot carry an author**, so
/// [`WriteIntent`]'s identity is the only source of a builder's author and
/// the author/identity mismatch class is unrepresentable rather than
/// fail-closed.
///
/// `id` and `sig` are deliberately absent for the same structural reason:
/// both are derived from signed bytes, so both only mean anything on a
/// payload that already went through a signer. A caller who holds them holds
/// a signed event and hands it over as [`WritePayload::Signed`]; a builder
/// is by definition the half of the lifecycle before the signature.
///
/// "Filled in when absent" is not "not sayable": `created_at` stays settable
/// and is then kept verbatim, tags are arbitrary and reach the wire
/// unchanged, and no kind is validated against a whitelist. Nothing here
/// refuses anything — guardrails belong in composers and diagnostics, not as
/// refusals in the one universal type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventBuilder {
    /// The ONE thing a builder cannot exist without.
    pub kind: Kind,
    /// Caller-owned and arbitrary: not reordered, not normalised, not
    /// filtered down to the ones some module claims.
    pub tags: Vec<Tag>,
    pub content: String,
    /// `None` — the ordinary case — is stamped at acceptance, which is the
    /// only moment both after the app finished describing the event and
    /// before anything downstream depends on the bytes. `Some(ts)` is kept
    /// exactly: absent-then-stamped is fine, present-then-changed is
    /// impossible.
    pub created_at: Option<Timestamp>,
}

impl EventBuilder {
    /// The kind is the only constructor argument. Every other field starts
    /// empty or unstated and is either filled by NMP or set by a
    /// combinator; the fields are public, so a caller who prefers struct
    /// literal syntax can use it directly.
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            tags: Vec::new(),
            content: String::new(),
            created_at: None,
        }
    }

    /// State the content. A plain `&str`/`String` passes through unchanged;
    /// a [`crate::text!`] value ALSO carries the rows its inline references
    /// require, which are appended here so the rendered mention and the row
    /// that resolves it can never diverge (`crate::InterpolatedContent`).
    pub fn content(mut self, content: impl Into<crate::InterpolatedContent>) -> Self {
        let content = content.into();
        self.content = content.text;
        self.tags.extend(content.rows);
        self
    }

    /// Point at something, or append one exact row.
    ///
    /// Handed an entity ([`crate::RootScope`] — a `Row`, an event, a NIP-73
    /// external content id), this is the ONE door that fills what the library
    /// already knows: the letter from the entity's shape, the relay hint from
    /// what NMP observed, the author in the row's own author slot, the
    /// companion `p` row and the parent's carried mentions. It reads the
    /// TARGET's thread position and never the kind being composed
    /// (`crate::ThreadPosition`), and `crate::Modifiers` states the per-
    /// relationship differences additively.
    ///
    /// Handed a bare [`Tag`], it is the same exact escape hatch it has always
    /// been: appended in order, validated against nothing, reordered never.
    ///
    /// Both land in the same function, so dedup and hint-filling cannot drift
    /// between two internal paths — the caution NDK's two divergent reply
    /// branches supply.
    pub fn tag(mut self, target: impl crate::TagRows) -> Self {
        self.tags.extend(target.tag_rows());
        self
    }

    /// Compose a reply to `target` — see [`crate::reply_to`], which this
    /// forwards to so the verb is reachable where a caller already holds the
    /// builder type.
    pub fn reply_to<T: crate::RootScope>(target: &T) -> Self {
        crate::reply_to(target)
    }

    /// State the timestamp instead of having it stamped at acceptance —
    /// what an app importing older content does. NMP keeps it verbatim.
    pub fn created_at(mut self, created_at: Timestamp) -> Self {
        self.created_at = Some(created_at);
        self
    }
}

/// The event payload of a write intent. VISION P states signing and
/// publishing are ORTHOGONAL stages, not one linear lifecycle: a caller
/// that already holds a validly-signed event (e.g. republishing a
/// previously-signed private event to a recomputed relay set, ledger #6, or
/// sending a followee's note verbatim to an archive relay) supplies
/// `Signed` and skips `Effect::RequestSign` entirely, going straight to
/// routing; a caller describing an event supplies a builder and the reducer
/// stamps, freezes and requests the signer capability.
///
/// The three variants are exactly the three places an author can come from:
/// `Event` has none until identity resolution stamps one, `ReplaceableEdit`
/// likewise plus a precondition, and `Signed` carries its author in its
/// bytes. There is no fourth.
pub enum WritePayload {
    Event(EventBuilder),
    /// A whole-value replacement whose acceptance is conditional on the
    /// store still holding exactly `expected_base` at the write's
    /// replaceable/addressable coordinate. `None` means "there is still no
    /// local winner"; it never means that Nostr is globally empty.
    ///
    /// The precondition travels with the builder so a protocol module can
    /// compose one closed, race-free write value. It is checked inside the
    /// store's atomic acceptance transaction, before an intent/receipt id is
    /// allocated or any canonical row is changed — and, when the builder
    /// states no `created_at`, that same transaction stamps
    /// `max(clock, winner.created_at + 1)` against the very row it is
    /// comparing, so monotonicity is decided by the only component that
    /// knows the winner.
    ReplaceableEdit {
        builder: EventBuilder,
        expected_base: Option<EventId>,
    },
    /// One capability-owned, replayable operation whose complete optimistic
    /// event is derived synchronously at acceptance. The opaque value is
    /// minted by the supported NMP facade; applications do not supply replay
    /// authority, source timestamps, or an author through this arm.
    ReplaceableOperation(ReplaceableOperation),
    Signed(SignedEvent),
}

const MAX_REPLACEABLE_OPERATION_BYTES: usize = 64 * 1024;

/// Opaque mechanism payload for one registered, body-complete replaceable
/// operation.
///
/// This type is public only because it is carried by the closed
/// [`WritePayload`] enum. Its fields have no public accessors and the NMP
/// facade does not re-export it. A supported caller receives an unforgeable
/// registration handle from the engine and asks that handle to mint the
/// payload; `publish()` rejects an unknown registration instance before
/// custody. Capability helpers therefore cannot state replay-program ids,
/// source authority, contributor membership, or a candidate body themselves.
pub struct ReplaceableOperation {
    registration_instance: [u8; 16],
    original_source: UnsignedEvent,
    current: UnsignedEvent,
    operation: Vec<u8>,
}

impl ReplaceableOperation {
    /// Mechanism-only constructor used by the facade's registered handle.
    ///
    /// It is intentionally hidden from documentation rather than offered as
    /// a supported construction door. Knowing this function still grants no
    /// authority: the random registration instance must be live in the exact
    /// engine receiving the intent, and `publish()` validates that fact before
    /// invoking capability code or writing anything.
    #[doc(hidden)]
    pub fn from_registered_parts(
        registration_instance: [u8; 16],
        original_source: UnsignedEvent,
        current: UnsignedEvent,
        operation: Vec<u8>,
    ) -> Result<Self, ReplaceableOperationError> {
        original_source
            .verify_id()
            .map_err(|_| ReplaceableOperationError::OriginalSourceInvalid)?;
        current
            .verify_id()
            .map_err(|_| ReplaceableOperationError::CurrentInvalid)?;
        if original_source.pubkey != current.pubkey
            || original_source.kind != current.kind
            || original_source.tags.identifier() != current.tags.identifier()
        {
            return Err(ReplaceableOperationError::CoordinateChanged);
        }
        if operation.is_empty() {
            return Err(ReplaceableOperationError::OperationEmpty);
        }
        if operation.len() > MAX_REPLACEABLE_OPERATION_BYTES {
            return Err(ReplaceableOperationError::OperationTooLarge);
        }
        Ok(Self {
            registration_instance,
            original_source,
            current,
            operation,
        })
    }

    /// Mechanism-only decomposition at the generic engine boundary. No
    /// supported facade or protocol helper exposes this method.
    #[doc(hidden)]
    pub fn into_registered_parts(self) -> ([u8; 16], UnsignedEvent, UnsignedEvent, Vec<u8>) {
        (
            self.registration_instance,
            self.original_source,
            self.current,
            self.operation,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceableOperationError {
    OriginalSourceInvalid,
    CurrentInvalid,
    CoordinateChanged,
    OperationEmpty,
    OperationTooLarge,
}

impl std::fmt::Display for ReplaceableOperationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::OriginalSourceInvalid => "replaceable operation original source is invalid",
            Self::CurrentInvalid => "replaceable operation current event is invalid",
            Self::CoordinateChanged => "replaceable operation source coordinate changed",
            Self::OperationEmpty => "replaceable operation must not be empty",
            Self::OperationTooLarge => "replaceable operation exceeds the 65536-byte bound",
        })
    }
}

impl std::error::Error for ReplaceableOperationError {}

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

/// The identity one write publishes under.
///
/// Exactly two words, and neither of them is an absence. [`Active`] is a
/// positive resolution instruction ("whoever is the active account when
/// this write is accepted"), not the lack of a choice — it can succeed,
/// fail, and be pinned, and it is what shows up in receipts and
/// diagnostics where a blank would say nothing.
///
/// What either word MEANS depends on whether the payload already states an
/// author, and the difference is the point: **where an author is absent,
/// identity SELECTS; where an author is stated, identity may only
/// RESTATE.** See [`WriteIntent::identity`] for the per-payload contract.
///
/// [`Active`]: Identity::Active
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Identity {
    /// Whoever is the active account at acceptance time. The default, and
    /// the overwhelming majority of writes: an app that is logged in and
    /// posting as itself says nothing at all.
    #[default]
    Active,
    /// This key, active or not — including while fully logged out. Naming a
    /// key is a complete statement of intent on its own and borrows nothing
    /// from the session, so publishing as one of several held identities
    /// never requires making it the active one.
    ///
    /// Always a [`PublicKey`], never an `npub` or any other bech32 form:
    /// bech32 is how something is shown to a person or received from one,
    /// and an app that took an npub from a paste box decodes it at that
    /// boundary (`docs/internals/conventions/bech32-boundary.md`).
    Explicit(PublicKey),
}

/// A caller's publish request.
pub struct WriteIntent {
    pub payload: WritePayload,
    pub routing: WriteRouting,
    /// The identity this ONE write is published under, defaulting to
    /// [`Identity::Active`] ([`Identity`]'s own `Default`).
    ///
    /// For a builder payload ([`WritePayload::Event`],
    /// [`WritePayload::ReplaceableEdit`]) there is no author to compare
    /// against, so the identity SELECTS one and is its only source.
    /// [`Identity::Active`] resolves the CURRENT active account at
    /// acceptance and stamps it — failing closed pre-acceptance when no
    /// account is active, since an instruction that cannot resolve is a
    /// refusal, not a parked hope (nothing is pinned, so nothing may park).
    /// [`Identity::Explicit`] stamps its key.
    ///
    /// For [`WritePayload::Signed`] the author is already frozen in the
    /// bytes and no identity choice can change it, so there the identity
    /// may only RESTATE it. [`Identity::Active`] means the event's own
    /// author, whoever that is — a signed event needs no signer, so it
    /// imposes no active-account requirement at all. `Explicit(pk)` must
    /// EQUAL `Event.pubkey`: naming that author is a harmless restatement
    /// of consent, and naming anybody else is a contradiction with no
    /// correct resolution, so it fails closed BEFORE acceptance. (Routing
    /// is a separate axis and stays independent of all of this —
    /// republishing somebody else's signed event to your own archive relay
    /// is an `Explicit` route over a payload signed by a different pubkey.)
    ///
    /// `Explicit(pk)` is the caller's explicit consent to publish this one
    /// write as `pk` — a registered/secondary identity — WITHOUT changing
    /// the active account. Acceptance pins the RESOLVED key into the frozen
    /// write (`expected_pubkey` / `signing_identity_ref`) either way, so
    /// later current-account changes can never retarget an accepted
    /// intent; under `Active` that pin matters more, not less, since
    /// acceptance is the only place "whoever is active" becomes somebody.
    /// An `Explicit` identity with no registered signing capability still
    /// ACCEPTS and parks durably (`WriteFact::AwaitingCapability`) until
    /// that exact key's signer attaches — never a silent failure, never
    /// identity drift.
    pub identity: Identity,
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
///
/// The whole app-facing routing vocabulary is these two words
/// (`docs/internals/routing/auto-and-explicit.md`). A routing value is a
/// STRATEGY, not a resolved relay set: it is stored durably and re-executed
/// at every send opportunity — first attempt, boot recovery, queue drain —
/// against whatever the engine knows at that moment. Nothing about
/// resolution logic is ever serialized.
///
/// Routing is independent of authorship. Republishing someone else's
/// already-signed event, unchanged, to your own archive relay is an
/// `Explicit` route chosen by the publishing user over a payload signed by a
/// different pubkey; nothing here derives a route from an identity or gates
/// a route by one.
#[derive(Clone)]
pub enum WriteRouting {
    /// "Figure out how to route whatever I'm publishing." NMP derives the
    /// route from the event at send time; the caller names no relay and no
    /// strategy.
    Auto,
    /// "Use these exact relays and that is that, no matter what else
    /// happens."
    ///
    /// Ledger #6's fail-closed discipline lives here, structurally:
    ///
    /// - **Verbatim execution.** Resolution yields exactly these relays,
    ///   every time. The directory is never consulted, so there is nothing
    ///   for it to contribute, augment, or substitute.
    /// - **No widen path.** No operation anywhere adds a relay to an
    ///   accepted `Explicit` route.
    /// - **Empty is refused before acceptance.** A publish carrying an empty
    ///   set is rejected at the door — no intent, no journal row, no receipt
    ///   lifecycle — so an accepted `Explicit` always names at least one
    ///   relay.
    ///
    /// What deliberately does NOT live here is a privacy claim. Fail-closed
    /// is a routing property; a group host and an archive relay are public
    /// targets, and calling an exact route "private" was the category error
    /// of the route this replaces
    /// (`docs/internals/routing/removed-routes.md`).
    Explicit(Vec<RelayUrl>),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1124 (PROTOCOL-WHATTHEAPPNEVERDOES-003, general-capabilities half):
    /// the claim that a group write cannot set the `h` context tag is about
    /// the semantic NIP-29 door specifically, not about `EventBuilder`
    /// throughout the repository. The ordinary builder validates nothing —
    /// `tag(Tag)` is the one intentional exact/raw escape (#1034's audits)
    /// and stays exactly that permissive outside a `Group`, including for a
    /// tag shaped like `h`.
    #[test]
    fn the_ordinary_builder_accepts_an_h_shaped_tag_with_no_validation() {
        let built = EventBuilder::new(nostr::Kind::TextNote)
            .tag(Tag::parse(["h", "anything-at-all"]).unwrap());
        assert_eq!(
            built.tags[0].as_slice(),
            &["h".to_string(), "anything-at-all".to_string()],
            "the general escape hatch is not aware of, and does not refuse, an h row"
        );
    }

    /// The routing vocabulary is exactly two words, and `Explicit` carries
    /// the caller's relay list verbatim — same order, same entries, nothing
    /// added. There is no third variant to reach for and no widen operation
    /// on the value the caller handed over.
    ///
    /// The division of labour is in the SHAPES, which is why this is a
    /// compile-time statement as much as a runtime one (#1105): `Auto` is a
    /// unit variant, so it is structurally incapable of carrying a relay a
    /// caller chose — it can only mean "derive it at send time"; `Explicit`
    /// is the only variant that holds relays, so caller-chosen destinations
    /// have exactly one spelling. Adding a third variant breaks the
    /// exhaustive match below rather than passing unnoticed. The same
    /// cardinality is enforced across the FFI, Swift and Kotlin surfaces by
    /// `scripts/check-routing-vocabulary.sh`.
    #[test]
    fn routing_is_two_words_and_explicit_is_verbatim() {
        let a = RelayUrl::parse("wss://a.example.com").unwrap();
        let b = RelayUrl::parse("wss://b.example.com").unwrap();

        let strategy_derived = WriteRouting::Auto;
        match strategy_derived {
            WriteRouting::Auto => {}
            WriteRouting::Explicit(_) => panic!("constructed Auto"),
        }

        let routing = WriteRouting::Explicit(vec![b.clone(), a.clone()]);
        match routing {
            WriteRouting::Explicit(relays) => assert_eq!(relays, vec![b, a]),
            WriteRouting::Auto => panic!("constructed Explicit"),
        }
    }

    /// The identity vocabulary is exactly two words, and neither is an
    /// absence: `Active` is what a caller gets by default (`Identity`'s own
    /// `Default`) and says something — "whoever is active at acceptance" —
    /// rather than saying nothing. Resolution lives in the reducer
    /// (`on_publish`), not here; this pins the vocab shape.
    #[test]
    fn identity_is_two_words_and_active_is_the_default() {
        let keys = nostr::Keys::generate();
        let builder = EventBuilder::new(nostr::Kind::TextNote).content("identity vocab");
        let default_intent = WriteIntent {
            payload: WritePayload::Event(builder.clone()),
            routing: WriteRouting::Auto,
            identity: Identity::default(),
            correlation: None,
        };
        assert_eq!(default_intent.identity, Identity::Active);

        let named = WriteIntent {
            payload: WritePayload::Event(builder),
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(keys.public_key()),
            correlation: None,
        };
        assert_eq!(named.identity, Identity::Explicit(keys.public_key()));
    }

    /// The kind is the only constructor argument, the combinators are
    /// consuming, and an unstated `created_at` stays `None` for the engine
    /// to stamp. There is no field, and no combinator, for an author.
    #[test]
    fn a_kind_alone_is_a_complete_builder() {
        let bare = EventBuilder::new(nostr::Kind::TextNote);
        assert_eq!(bare.kind, nostr::Kind::TextNote);
        assert!(bare.tags.is_empty());
        assert_eq!(bare.content, "");
        assert_eq!(bare.created_at, None);

        let stated = EventBuilder::new(nostr::Kind::Custom(31337))
            .content("hello")
            .tag(Tag::parse(["client", "nobody-registered"]).unwrap())
            .tag(Tag::parse(["zzz", "a value with spaces"]).unwrap())
            .created_at(Timestamp::from(1_551_691_700));
        assert_eq!(stated.kind, nostr::Kind::Custom(31337));
        assert_eq!(stated.content, "hello");
        assert_eq!(stated.created_at, Some(Timestamp::from(1_551_691_700)));
        assert_eq!(
            stated
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect::<Vec<_>>(),
            vec![
                vec!["client".to_string(), "nobody-registered".to_string()],
                vec!["zzz".to_string(), "a value with spaces".to_string()],
            ]
        );
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
