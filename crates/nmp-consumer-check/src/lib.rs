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
//! - the advertised write path ([`build_event_intent`]) -- `EventBuilder`
//!   plus `Kind`/`Tag`/`Timestamp`, the exact re-exports a prior review
//!   found missing, and now also the proof that publishing with the current
//!   account takes a kind and content and nothing else;
//! - naming every `DiagnosticsSnapshot` output type, not just some of them
//!   ([`describe_snapshot`]/[`describe_relay`]/[`describe_coverage_entry`]) --
//!   `DiagnosticsSnapshot`, `RelayDiagnosticsSnapshot`, `FilterCoverageEntry`,
//!   `CoverageInterval` (the engine-global diagnostics watermark type
//!   `FilterCoverageEntry.coverage` now carries), and `Lane` are each named
//!   explicitly. The distinct query-facing `AcquisitionEvidence` type is named
//!   by [`describe_evidence`], so both halves of the read surface are closure-
//!   checked from an `nmp`-only dependency rather than merely imported.
//!   imported and left unused past one field read.
//! - NIP-22 comment composition ([`build_comment_intent`]) -- the vocabulary
//!   #851 moved behind this facade so `nmp-ffi` could drop its direct
//!   `nmp-nip22` edge. It is the exact value the FFI projection composes,
//!   proving one owner rather than two aligned by convention.
//! - every protocol/content family #1239 retrofitted onto the facade
//!   ([`compose_every_retrofitted_family`]) -- NIP-C7 chat, NIP-18 reposts,
//!   NIP-25 reactions, NIP-51 simple groups, content parsing, exact-byte asset
//!   identity and Blossom. `nmp-ffi` bound all of them directly and the facade
//!   offered none, so a Swift app got them by linking one staticlib while a
//!   direct-Rust app named six more crates. This crate's `Cargo.toml` still
//!   names `nmp` alone.
//! - NIP-02 follow/unfollow ([`follow_someone`]) -- #1143's retrofit, and the
//!   one family that took a package-graph inversion rather than a feature
//!   flag: `nmp-nip02` used to depend on `nmp`, the only upward edge in the
//!   workspace's dependency graph, so a direct-Rust app could not reach the
//!   follow door through `nmp` at all. The `#[cfg(test)]` module below
//!   drives it against a real `Engine`, proving usable, not just nameable.
//!
//! The `#[cfg(test)]` module below additionally drives a real `Engine`
//! end-to-end (construct, `add_private_key_account`, `observe`, `publish`,
//! `observe_diagnostics`, `shutdown`) with no relays configured -- proving
//! the two nouns are not just nameable but usable.

use nmp::{
    AcquisitionEvidence, AuthDiagnosticsPhase, AuthDiagnosticsSnapshot, CoverageInterval, Demand,
    Derived, DiagnosticsSnapshot, Engine, Event, EventBuilder, Filter, FilterCoverageEntry,
    Identity, IdentityField, IndexedTagName, Kind, Lane, LiveQuery, NostrEntity,
    ObservationEvidence, PublicKey, ReceiptStream, RelayDiagnosticsSnapshot, RelayUrl, Selector,
    Tag, Timestamp, WriteIntent, WritePayload, WriteRouting,
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

/// Proves a builder `WriteIntent` is fully constructible from `nmp` alone
/// -- the advertised write path (`EventBuilder`/`Kind`/`Tag`). Uses the
/// same arbitrary caller-owned content kind as
/// [`build_derived_index_query`], never a NIP-01 core kind. It supplies a
/// kind and content and NOTHING else: no pubkey, no timestamp, no id. That
/// is the proof -- a direct-Rust app cannot state an author here even if it
/// wanted to. Composes the default identity contract
/// (`Identity::Active`, #47), so this intent signs as the current account;
/// the per-write `Identity::Explicit` spelling is likewise reachable from
/// `nmp` alone for callers that need it.
pub fn build_event_intent(content: &str) -> WriteIntent {
    WriteIntent {
        payload: WritePayload::Event(
            EventBuilder::new(Kind::Custom(CALLER_CONTENT_KIND)).content(content),
        ),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }
}

/// The explicit-identity half of the write path (#47): publishing as a
/// specific non-current account takes an `Identity` and nothing else -- there
/// is still no pubkey anywhere inside the payload, because a builder has no
/// field for one.
pub fn build_event_intent_as(identity: PublicKey, content: &str) -> WriteIntent {
    WriteIntent {
        payload: WritePayload::Event(
            EventBuilder::new(Kind::Custom(CALLER_CONTENT_KIND)).content(content),
        ),
        routing: WriteRouting::Auto,
        identity: Identity::Explicit(identity),
        correlation: None,
    }
}

/// The other half of the builder proof: "NMP fills what you left unsaid" is
/// not "you cannot say it". This one states an arbitrary `Tag` and an
/// explicit `Timestamp` -- both reachable from `nmp` alone -- and NMP keeps
/// the timestamp verbatim rather than restamping it.
pub fn build_dated_event_intent(created_at: Timestamp, tag: Tag, content: &str) -> WriteIntent {
    WriteIntent {
        payload: WritePayload::Event(
            EventBuilder::new(Kind::Custom(CALLER_CONTENT_KIND))
                .content(content)
                .tag(tag)
                .created_at(created_at),
        ),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
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

/// Names the NIP-22 comment vocabulary and composes its write operation from
/// `nmp` alone (#851): `nmp::nip22` is the ONE owner, so a direct-Rust app
/// reaches exactly what `nmp-ffi`'s `comment_intent` projection reaches --
/// neither needs an `nmp-nip22` line of its own. What comes back is an
/// ordinary [`WriteIntent`] (#907), published through the same
/// `Engine::publish` lifecycle as any other write. Uses an external NIP-73
/// content id so no NIP-01 core kind is baked into this proof. The vocabulary is
/// engine-free and still reads no ambient clock or current account -- it no
/// longer needs an author or an event time to say so, because the engine
/// resolves both at acceptance.
pub fn build_comment_intent(
    guid: &str,
    content: &str,
) -> Result<WriteIntent, nmp::nip22::Nip73Error> {
    let root = nmp::nip22::CommentRoot::External(nmp::nip22::Nip73::podcast_episode(guid)?);
    Ok(nmp::nip22::comment_intent(&root, content.to_string(), None))
}

/// Names every protocol/content family #1239 retrofitted onto the facade, from
/// `nmp` alone.
///
/// This is the acceptance proof for that issue, and it is deliberately one
/// function rather than six: the claim is not "each family compiles" but "an
/// app reaching all of them still names `nmp` alone", and only a single
/// `Cargo.toml` with no second crate in it can say that. Before #1239 the same
/// code needed six more dependency lines that a Swift app never needed,
/// because `nmp-ffi` bound the crates directly and the facade offered nothing.
///
/// Every door here composes and returns rather than merely being imported, so
/// removing any one re-export breaks this crate instead of leaving a stale
/// claim in a doc comment. `nip02` is proven separately, by
/// [`follow_someone`] and this crate's `#[cfg(test)]` module: its write door
/// needs a live `Engine`, not a target event, so it does not fit this
/// function's pure-composition shape.
pub fn compose_every_retrofitted_family(target: &Event, source: Option<RelayUrl>) -> Vec<String> {
    // NIP-C7 kind:9 chat, top-level and threaded (`nmp::nipc7`).
    let chat = nmp::nipc7::chat();
    let chat_reply = nmp::nipc7::chat_reply(target);
    // NIP-18 repost, whose whole value is that the caller never picks between
    // kind:6 and kind:16 (`nmp::nip18`).
    let repost = nmp::nip18::repost(target, source.clone());
    // NIP-25 reaction (`nmp::nip25`), wired at birth by #155 and named here so
    // the retrofit and the family that avoided it are proven the same way.
    let reaction = nmp::nip25::react(target, source, nmp::nip25::Reaction::Like);
    // NIP-51 kind:10009: the demand that reads it and the tolerant codec that
    // decodes what came back (`nmp::nip29`).
    let groups_demand: Demand = nmp::nip29::current_account_group_list_demand();
    let groups: nmp::nip29::SimpleGroupsList =
        nmp::nip29::parse_simple_groups_list_tolerant(target);
    let first_group: Option<&nmp::nip29::SimpleGroupEntry> = groups.items.first();
    // Content parsing (`nmp::content`) -- the door mosaico hand-rolled a
    // `find("nostr:")` scanner for, because it could not reach this one.
    let document: nmp::content::ContentDocument =
        nmp::content::parse_content(&target.content, nmp::content::ContentSyntax::PlainText);
    let references: Vec<&NostrEntity> = document
        .references()
        .into_iter()
        .map(|occurrence: &nmp::content::ReferenceOccurrence| &occurrence.target)
        .collect();
    // Exact-byte identity and the Blossom vocabulary built on it
    // (`nmp::asset`, `nmp::blossom`).
    let digest: nmp::asset::Sha256Hash = nmp::asset::Sha256Hash::of(target.content.as_bytes());
    let verbs = [
        nmp::blossom::BlossomVerb::Upload,
        nmp::blossom::BlossomVerb::Delete,
        nmp::blossom::BlossomVerb::List,
    ];

    vec![
        format!("chat {:?}", chat.kind),
        format!(
            "chat_reply {:?} +{} row(s)",
            chat_reply.kind,
            chat_reply.tags.len()
        ),
        format!("repost {:?}", repost.kind),
        format!("reaction {:?} {:?}", reaction.kind, reaction.content),
        format!("groups demand {groups_demand:?}"),
        format!(
            "{} group(s), first {:?}, {} malformed",
            groups.items.len(),
            first_group.map(|entry| &entry.group_id),
            groups.malformed_item_count,
        ),
        format!(
            "{} reference(s) in {} block(s)",
            references.len(),
            document.blocks.len()
        ),
        digest.to_hex(),
        format!("{verbs:?}"),
    ]
}

/// The demand `nmp::nip02` reads the current account's kind:3 contact list
/// through -- reachable from `nmp` alone since #1143 closed the reverse
/// `nmp-nip02 -> nmp` edge.
#[must_use]
pub fn follow_demand() -> Demand {
    nmp::nip02::current_account_demand()
}

/// Follows `target` through the ordinary NIP-02 write door, entirely from
/// `nmp`. This is #1143's acceptance proof: before the fix, a direct-Rust
/// app could not reach `set_following` through this facade at all --
/// `nmp-nip02` depended on `nmp`, so an app wanting to follow someone had to
/// name a second, upward-pointing crate. `writes` composes once (typically
/// held for the process lifetime); this function takes it by reference so a
/// caller following many targets does not re-register the capability handle
/// each time.
pub fn follow_someone(
    engine: &Engine,
    writes: &nmp::nip02::FollowWrites,
    target: PublicKey,
) -> Result<ReceiptStream, nmp::nip02::FollowActionFailure> {
    nmp::nip02::set_following(engine, writes, target, nmp::nip02::FollowChange::Follow)
}

/// The unfollow half of [`follow_someone`], proving both directions of
/// [`nmp::nip02::FollowChange`] are reachable from `nmp` alone.
pub fn unfollow_someone(
    engine: &Engine,
    writes: &nmp::nip02::FollowWrites,
    target: PublicKey,
) -> Result<ReceiptStream, nmp::nip02::FollowActionFailure> {
    nmp::nip02::set_following(engine, writes, target, nmp::nip02::FollowChange::Unfollow)
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
    use nmp::EngineConfig;

    /// A fixed, valid secp256k1 secret key -- generated once via `openssl
    /// rand -hex 32`. Hardcoded rather than derived from `nostr::Keys`
    /// because this crate has no dependency on `nostr` at all (the whole
    /// point of this crate).
    const TEST_SECRET_KEY_BYTES: [u8; 32] = [
        50, 246, 223, 115, 234, 216, 80, 182, 225, 60, 6, 73, 132, 107, 122, 29, 150, 70, 214, 160,
        181, 12, 105, 54, 25, 129, 23, 110, 129, 126, 112, 248,
    ];

    /// Drives `Engine::new`/session account operations/`observe`/`publish`/
    /// `observe_diagnostics`/`shutdown` end-to-end from this `nmp`-only
    /// crate, with no relays configured (no network needed) -- the two
    /// nouns are not merely nameable, they are usable.
    #[test]
    fn engine_two_nouns_are_usable_from_nmp_alone() {
        let engine =
            Engine::new(EngineConfig::default()).expect("temporary Redb engine must build");

        let account = engine
            .add_private_key_account(&TEST_SECRET_KEY_BYTES, true)
            .expect("fixed decoded test secret key must validate");

        let subscription = engine
            .observe(build_derived_index_query(), None)
            .expect("engine is open");
        drop(subscription); // explicit early withdraw, exercised via Drop

        let receipts = engine
            .publish(build_event_intent("hello from an nmp-only consumer"))
            .expect("engine is open");
        drop(receipts);

        let diagnostics = engine.observe_diagnostics().expect("engine is open");
        if let Some(snapshot) = diagnostics.recv() {
            let _ = describe_snapshot(&snapshot);
        }

        // The session account is removed as a whole, and a second removal is
        // a `false` no-op.
        assert!(engine
            .remove_account(&account)
            .expect("remove_account must be reachable from nmp alone"));
        assert!(!engine
            .remove_account(&account)
            .expect("repeated removal must no-op, not error"));

        engine.shutdown();
    }

    /// #1143 acceptance proof: a direct-Rust app follows and unfollows
    /// through this `nmp`-only facade, and opens the reactive contact-list
    /// demand [`follow_demand`] reads it through -- the exact user story
    /// that was unreachable before the follow door's package-graph
    /// inversion (`nmp-nip02` used to depend on `nmp`, so an app wanting to
    /// follow someone had to name that second, upward-pointing crate).
    #[test]
    fn follow_and_unfollow_are_usable_from_nmp_alone() {
        let engine = Engine::new_with_capabilities(
            EngineConfig::default(),
            vec![nmp::nip02::follow_capability()],
        )
        .expect("temporary Redb engine must build");
        engine
            .add_private_key_account(&TEST_SECRET_KEY_BYTES, true)
            .expect("fixed decoded test secret key must validate");
        let writes = nmp::nip02::follow_writes();
        let target: PublicKey = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
            .parse()
            .expect("fixed public key must parse");

        let subscription = engine
            .observe(LiveQuery::single(follow_demand()), None)
            .expect("the reactive contact-list demand is reachable and usable from nmp alone");
        drop(subscription);

        let follow_receipt = follow_someone(&engine, &writes, target)
            .expect("follow must enter ordinary custody from nmp alone");
        drop(follow_receipt);
        assert_eq!(
            engine.publish_queue(None, 10).unwrap().len(),
            1,
            "the follow write reached ordinary custody"
        );

        let unfollow_receipt = unfollow_someone(&engine, &writes, target)
            .expect("unfollow must enter ordinary custody from nmp alone");
        drop(unfollow_receipt);
        assert_eq!(
            engine.publish_queue(None, 10).unwrap().len(),
            2,
            "the unfollow write reached ordinary custody as its own entry"
        );

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

        let engine =
            Engine::new(EngineConfig::default()).expect("temporary Redb engine must build");
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
}
