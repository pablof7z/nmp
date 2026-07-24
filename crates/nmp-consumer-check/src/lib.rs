//! External-consumer closure proof for the `nmp` facade (#52 acceptance:
//! "an app's `Cargo.toml` names `nmp` alone"). This crate's own
//! `Cargo.toml` depends on `nmp` ONLY -- no mechanism crate, and not even
//! `nostr` directly: every value type below is reached through `nmp`'s own
//! re-exports. If this crate fails to compile, the facade's re-export
//! inventory has a gap.
//!
//! Every fixture here uses ARBITRARY caller-owned kinds (9998/9999), never
//! kind:1/kind:3 or any other NIP-01 core schema. `docs/known-gaps.md`'s v2
//! contract promotion is explicit: "No kind:1-first core catalog is part of
//! the target" -- a facade acceptance proof that hardcodes the
//! follows/feed shape would bake exactly the kind bias that promotion
//! forbids into the canonical surface's own story. Everything below proves
//! the GRAMMAR/write-plane/diagnostics MECHANICS are reachable from `nmp`
//! alone; it asserts nothing about what any particular kind means.
//!
//! Exercises, from `nmp` alone:
//! - the grammar a `LiveQuery` is built from ([`build_derived_index_query`]);
//! - the advertised unsigned-write path ([`build_unsigned_intent`]) --
//!   `UnsignedEvent`/`Kind`/`Tag`/`Timestamp` were the exact re-exports a
//!   prior review found missing;
//! - naming every `DiagnosticsSnapshot` output type, not just some of them
//!   ([`describe_snapshot`]/[`describe_relay`]/[`describe_coverage_entry`]) --
//!   `DiagnosticsSnapshot`, `RelayDiagnosticsSnapshot`, `FilterCoverageEntry`,
//!   `CoverageInterval` (the engine-global diagnostics watermark type
//!   `FilterCoverageEntry.coverage` now carries), and `Lane` are each named
//!   explicitly. The distinct query-facing `AcquisitionEvidence` type is named
//!   by [`describe_evidence`], so both halves of the read surface are closure-
//!   checked from an `nmp`-only dependency rather than merely imported.
//!   imported and left unused past one field read.
//!
//! The `#[cfg(test)]` module below additionally drives a real `Engine`
//! end-to-end (construct, `add_account`, `observe`, `publish`,
//! `observe_diagnostics`, `shutdown`) with no relays configured -- proving
//! the two nouns are not just nameable but usable.

use nmp::{
    AcquisitionEvidence, AuthDiagnosticsPhase, AuthDiagnosticsSnapshot, CoverageInterval, Demand,
    Derived, DiagnosticsSnapshot, Durability, Filter, FilterCoverageEntry, IdentityField,
    IndexedTagName, Kind, Lane, LiveQuery, ObservationEvidence, PublicKey,
    RelayDiagnosticsSnapshot, Selector, Tag, Timestamp, UnsignedEvent, WriteIntent, WritePayload,
    WriteRouting,
};

/// The reactive index kind an app might declare its own membership list
/// under -- arbitrary, caller-owned, and meaningless to `nmp` itself.
const CALLER_INDEX_KIND: u16 = 9998;
/// The content kind that index's projected tag identifies authors of --
/// likewise arbitrary and caller-owned.
const CALLER_CONTENT_KIND: u16 = 9999;

/// A caller-owned derived-index query shape: kind `9999` content authored
/// by whoever the active pubkey's kind `9998` "index" event currently names
/// via its `p` tags, restricted to one arbitrary caller-owned `#d` group.
/// Structurally identical to the reactive-derived-set shape this repo's
/// other falsifiers build (a `Derived`/`Reactive`/`Tag`-projection), just
/// re-kinded onto two arbitrary caller-owned kinds instead of any NIP-01
/// core schema -- proves `Filter`/`Binding`/`Derived`/`Selector`/
/// `IdentityField`/`IndexedTagName` are all nameable and constructible from
/// `nmp` alone, without asserting anything about what a specific kind
/// means. `Selector::Tag` takes an arbitrary `String` key (the local
/// event-tag projection, #64) while the inner `Filter.tags` entry is keyed
/// by `IndexedTagName` (the wire/local indexed-filter alphabet, #64) --
/// deliberately exercising both halves of that split from this crate alone.
pub fn build_derived_index_query() -> LiveQuery {
    LiveQuery::from_filter(Filter {
        kinds: Some(std::collections::BTreeSet::from([CALLER_CONTENT_KIND])),
        authors: Some(nmp::Binding::Derived(Box::new(Derived {
            inner: Demand::from_filter(Filter {
                kinds: Some(std::collections::BTreeSet::from([CALLER_INDEX_KIND])),
                authors: Some(nmp::Binding::Reactive(IdentityField::ActivePubkey)),
                tags: std::collections::BTreeMap::from([(
                    IndexedTagName::new('d').expect("'d' is a valid ASCII-letter indexed tag key"),
                    nmp::Binding::Literal(std::collections::BTreeSet::from([
                        "arbitrary-caller-group".to_string(),
                    ])),
                )]),
                ..Filter::default()
            }),
            project: Selector::Tag("p".to_string()),
        }))),
        ..Filter::default()
    })
}

/// Proves an unsigned `WriteIntent` is fully constructible from `nmp` alone
/// -- the advertised unsigned-write path (`UnsignedEvent`/`Kind`/`Tag`/
/// `Timestamp`). Uses the same arbitrary caller-owned content kind as
/// [`build_derived_index_query`], never a NIP-01 core kind. Composes the
/// default identity contract (`identity_override: None`, #47): this intent
/// signs as the active account, and the per-write override field is
/// likewise reachable from `nmp` alone for callers that need it.
pub fn build_unsigned_intent(author: PublicKey, content: &str) -> WriteIntent {
    let unsigned = UnsignedEvent::new(
        author,
        Timestamp::now(),
        Kind::Custom(CALLER_CONTENT_KIND),
        Vec::<Tag>::new(),
        content,
    );
    WriteIntent {
        payload: WritePayload::Unsigned(unsigned),
        durability: Durability::Ephemeral,
        routing: WriteRouting::AuthorOutbox,
        identity_override: None,
        correlation: None,
    }
}

/// Names `FilterCoverageEntry` AND `CoverageInterval` as explicit types (not
/// merely a field read through `Debug`) -- proves both resolve from `nmp`
/// alone. `CoverageInterval` is the engine-global DIAGNOSTICS watermark type
/// `FilterCoverageEntry.coverage` carries post-#49 -- deliberately distinct
/// from the scoped, per-query `AcquisitionEvidence` [`describe_evidence`]
/// names below (`docs/design/scoped-evidence-49-12-plan.md` §4: the two
/// surfaces are never conflated).
pub fn describe_coverage_entry(entry: &FilterCoverageEntry) -> String {
    let coverage: &Option<CoverageInterval> = &entry.coverage;
    format!("{}: {coverage:?}", entry.filter)
}

/// Names `AcquisitionEvidence` as an explicit type -- the scoped, per-query
/// evidence `nmp::Subscription::recv`'s `Frame` carries alongside every
/// row batch (never engine-global, never a completeness verdict). Mirrors
/// [`describe_coverage_entry`]'s "explicit type, not just a `Debug` field
/// read" proof for the diagnostics side, but for the query-observation
/// side instead -- so removing `AcquisitionEvidence` from `nmp`'s
/// re-exports breaks this crate too, not just the diagnostics half of the
/// facade surface.
pub fn describe_evidence(evidence: &AcquisitionEvidence) -> String {
    format!(
        "{} source(s), {} shortfall fact(s)",
        evidence.sources.len(),
        evidence.shortfall.len()
    )
}

/// Names and reads the observation-scoped execution envelope from an
/// `nmp`-only dependency. The stable kind/path/revision fields drive cascade
/// presentation; exact values/fingerprint and ordered attributes retain the
/// resolver or transport owner's supporting evidence.
pub fn describe_execution(evidence: &ObservationEvidence) -> String {
    format!(
        "#{} {} {:?}@{:?}: {} value(s), fingerprint {:?}, attributes {:?}",
        evidence.sequence,
        evidence.kind,
        evidence.path,
        evidence.revision,
        evidence.values.len(),
        evidence.fingerprint,
        evidence.attributes,
    )
}

/// Names `RelayDiagnosticsSnapshot` and `Lane` as explicit types, and calls
/// through to [`describe_coverage_entry`] for every one of its coverage
/// entries -- so removing ANY of `RelayDiagnosticsSnapshot`/`Lane`/
/// `FilterCoverageEntry`/`CoverageInterval` from `nmp`'s re-exports breaks
/// this crate, not just a claim in a doc comment.
pub fn describe_relay(snapshot: &RelayDiagnosticsSnapshot) -> String {
    let lanes: Vec<Lane> = snapshot.by_lane.iter().map(|(lane, _)| *lane).collect();
    let coverage: Vec<String> = snapshot
        .coverage
        .iter()
        .map(describe_coverage_entry)
        .collect();
    format!(
        "{} subs on {} across lanes {lanes:?}; coverage: [{}]",
        snapshot.wire_sub_count,
        snapshot.relay,
        coverage.join(", "),
    )
}

/// Names `AuthDiagnosticsSnapshot` AND `AuthDiagnosticsPhase` (#8 Wave 5's
/// facade-owned per-session AUTH read-out) as explicit types, exhaustively
/// matching every phase -- so dropping either export, or a phase variant,
/// breaks this `nmp`-only crate rather than merely a doc claim.
pub fn describe_auth_session(session: &AuthDiagnosticsSnapshot) -> String {
    let phase: AuthDiagnosticsPhase = session.phase;
    let phase = match phase {
        AuthDiagnosticsPhase::AwaitingChallenge => "awaiting challenge",
        AuthDiagnosticsPhase::AwaitingPolicy => "awaiting policy",
        AuthDiagnosticsPhase::AwaitingSignature => "awaiting signature",
        AuthDiagnosticsPhase::AwaitingSend => "awaiting send",
        AuthDiagnosticsPhase::AwaitingRelayAck => "awaiting relay ack",
        AuthDiagnosticsPhase::Ready => "ready",
        AuthDiagnosticsPhase::Denied => "denied",
        AuthDiagnosticsPhase::Error => "error",
    };
    format!(
        "{} gen {} epoch {:?}: {phase}",
        session.relay, session.transport_generation, session.epoch_sequence,
    )
}

/// Names the TOP-LEVEL `DiagnosticsSnapshot` type itself -- a prior version
/// of this proof imported `RelayDiagnosticsSnapshot`/`Lane` but never named
/// `DiagnosticsSnapshot` or `FilterCoverageEntry` anywhere, so the facade
/// could have dropped either re-export without this crate noticing. This
/// function's parameter type closes that gap. It also reads the documented
/// `auth_sessions` read-out through [`describe_auth_session`].
pub fn describe_snapshot(snapshot: &DiagnosticsSnapshot) -> String {
    let relays: Vec<String> = snapshot.relays.iter().map(describe_relay).collect();
    let auth: Vec<String> = snapshot
        .auth_sessions
        .iter()
        .map(describe_auth_session)
        .collect();
    format!(
        "{} uncovered authors; relays: [{}]; auth sessions: [{}]",
        snapshot.uncovered_author_count,
        relays.join("; "),
        auth.join("; "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp::{
        Engine, EngineConfig, SignEventFailure, SignEventOperation, SignEventRequest,
        SignEventStartError, SignerError, SignerOp, SigningCapability,
    };

    /// A fixed, valid secp256k1 secret key -- generated once via `openssl
    /// rand -hex 32`. Hardcoded rather than derived from `nostr::Keys`
    /// because this crate has no dependency on `nostr` at all (the whole
    /// point of this crate).
    const TEST_SECRET_KEY_HEX: &str =
        "32f6df73ead850b6e13c0649846b7a1d9646d6a0b50c69361981176e817e70f8";

    struct ExternalAsyncSigner {
        public_key: PublicKey,
    }

    impl SigningCapability for ExternalAsyncSigner {
        fn public_key(&self) -> Option<PublicKey> {
            Some(self.public_key)
        }

        fn sign(&self, _unsigned: UnsignedEvent) -> SignerOp<nmp::Event> {
            let (completion, operation) = SignerOp::pending_channel();
            std::thread::spawn(move || {
                let _ = completion.resolve(Err(SignerError::Rejected(
                    "external asynchronous refusal".to_string(),
                )));
            });
            operation
        }
    }

    /// Drives `Engine::new`/`add_account`/`observe`/`publish`/
    /// `observe_diagnostics`/`shutdown` end-to-end from this `nmp`-only
    /// crate, with no relays configured (no network needed) -- the two
    /// nouns are not merely nameable, they are usable.
    #[test]
    fn engine_two_nouns_are_usable_from_nmp_alone() {
        let engine = Engine::new(EngineConfig::default()).expect("in-memory engine must build");

        let account = engine
            .add_account(TEST_SECRET_KEY_HEX)
            .expect("fixed test secret key must parse");
        let author = account.public_key();

        let subscription = engine
            .observe(build_derived_index_query(), None)
            .expect("engine is open");
        drop(subscription); // explicit early withdraw, exercised via Drop

        let receipts = engine
            .publish(build_unsigned_intent(
                author,
                "hello from an nmp-only consumer",
            ))
            .expect("engine is open");
        drop(receipts);

        let diagnostics = engine.observe_diagnostics().expect("engine is open");
        if let Some(snapshot) = diagnostics.recv() {
            let _ = describe_snapshot(&snapshot);
        }

        // #8's ratified account model: the registration detaches exactly the
        // installation it proves, and a second removal is a `false` no-op.
        assert!(engine
            .remove_account(&account)
            .expect("remove_account must be reachable from nmp alone"));
        assert!(!engine
            .remove_account(&account)
            .expect("stale removal must no-op, not error"));

        engine.shutdown();
    }

    /// An external crate can implement a genuinely asynchronous NIP-42 AUTH
    /// policy with only its `nmp` dependency (#8 Wave 5): the trait, the
    /// request getters, the ready/pending operation constructors, the
    /// pending completion door, and the exact-instance registration
    /// lifecycle are all reachable from `nmp` alone -- no channel crate and
    /// no mechanism crate appear in this manifest.
    #[test]
    fn external_auth_policy_needs_only_nmp() {
        use nmp::{
            AuthPolicy, AuthPolicyDecision, AuthPolicyError, AuthPolicyOp, AuthPolicyRequest,
            AuthPolicyResolveError,
        };

        struct ExternalAsyncPolicy;

        impl AuthPolicy for ExternalAsyncPolicy {
            fn evaluate(&self, request: AuthPolicyRequest) -> AuthPolicyOp {
                // The request's whole vocabulary is readable from nmp alone.
                let _identity: PublicKey = request.expected_pubkey();
                let _wire = format!(
                    "{} challenged {:?} (gen {}, epoch {})",
                    request.relay(),
                    request.challenge(),
                    request.transport_generation(),
                    request.epoch_sequence(),
                );
                let (completion, operation) = AuthPolicyOp::pending_channel();
                std::thread::spawn(move || {
                    let _ = completion.resolve(Ok(AuthPolicyDecision::Deny {
                        reason: "external asynchronous refusal".to_string(),
                    }));
                });
                operation
            }
        }

        // The ready constructors and both result arms are constructible.
        let _ready = AuthPolicyOp::allow();
        let _denied = AuthPolicyOp::deny("not now");
        let _failed = AuthPolicyOp::ready(Err(AuthPolicyError::Technical {
            reason: "policy backend offline".to_string(),
        }));

        // The pending door's typed refusals are observable.
        let (sender, operation) = AuthPolicyOp::pending_channel();
        drop(operation);
        assert!(matches!(
            sender.resolve(Ok(AuthPolicyDecision::Allow)),
            Err(AuthPolicyResolveError::ReceiverDropped(Ok(
                AuthPolicyDecision::Allow
            )))
        ));

        let engine = Engine::new(EngineConfig::default()).expect("in-memory engine must build");
        let identity: PublicKey =
            "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
                .parse()
                .expect("fixed public key must parse");
        let registration = engine
            .add_auth_policy(identity, ExternalAsyncPolicy)
            .expect("policy must register from nmp alone");
        assert_eq!(registration.expected_public_key(), identity);
        let replacement = engine
            .add_auth_policy(identity, ExternalAsyncPolicy)
            .expect("replacement must register");
        assert!(
            !engine
                .remove_auth_policy(&registration)
                .expect("stale removal must be a typed no-op"),
            "a stale registration must never detach its replacement"
        );
        assert!(engine
            .remove_auth_policy(&replacement)
            .expect("exact registration must detach"));
        engine.shutdown();
    }

    /// An external crate can implement a genuinely asynchronous signer with
    /// only its `nmp` dependency: no channel crate appears in this manifest
    /// or source, and the engine observes the typed result at runtime.
    #[test]
    fn external_async_signer_needs_only_nmp() {
        let engine = Engine::new(EngineConfig::default()).expect("in-memory engine must build");
        let author: PublicKey = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
            .parse()
            .expect("fixed public key must parse");
        engine
            .add_signer(ExternalAsyncSigner { public_key: author })
            .expect("external signer must register");
        engine
            .set_active_account(Some(author))
            .expect("external signer must become active");

        let start: Result<SignEventOperation, SignEventStartError> =
            engine.sign_event(SignEventRequest {
                created_at: Timestamp::from(42),
                kind: Kind::Custom(CALLER_CONTENT_KIND),
                tags: Vec::new(),
                content: "external asynchronous signing".to_string(),
            });
        let result: Result<nmp::Event, SignEventFailure> =
            start.expect("sign operation must be accepted").recv();

        assert_eq!(
            result,
            Err(SignEventFailure::SignerRejected {
                reason: "external asynchronous refusal".to_string(),
            })
        );
        engine.shutdown();
    }
}
