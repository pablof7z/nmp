//! External-consumer closure proof for the `nmp` facade (#52 acceptance:
//! "an app's `Cargo.toml` names `nmp` alone" for the GENERIC engine surface
//! -- custody, storage, routing, signing, delivery, recovery, receipts).
//! This crate's `Cargo.toml` depends on `nmp` for every one of those nouns
//! and nothing else in that half -- no mechanism crate, and not even
//! `nostr` directly. `nmp` never re-exports a capability's own meaning
//! (#1707), so every capability -- `nmp-nip02`, `nmp-nip18`, `nmp-nip22`,
//! `nmp-nip25`, `nmp-nipc7`, `nmp-content`, `nmp-asset`, `nmp-blossom` --
//! is its own SEPARATE, EXPLICIT dependency line below, not a facade
//! feature. If a generic engine noun ever needs a second `use` line naming
//! a mechanism crate or `nostr` itself, the facade's re-export inventory
//! has a gap; if a capability crate needs a second line, that is the
//! target shape working as designed, not a gap.
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
//! - NIP-22 comment composition ([`build_comment_intent`]) -- reachable
//!   through `nmp-nip22`, an EXPLICIT dependency (#1707 reversed #851's
//!   absorption of the comment vocabulary into this facade).
//! - every other protocol/content family #1239 once retrofitted onto the
//!   facade ([`compose_every_retrofitted_family`]) -- NIP-C7 chat, NIP-18
//!   reposts, NIP-25 reactions, content parsing, exact-byte asset identity
//!   and Blossom -- each reachable the same way: its own explicit
//!   `nmp-nip18`/`nmp-nipc7`/`nmp-nip25`/`nmp-content`/`nmp-asset`/
//!   `nmp-blossom` dependency line, not a facade feature (#1707 deleted
//!   each of these eight pure re-export doors -- no engine coupling, so
//!   nothing forced them into `nmp` in the first place).
//! - NIP-02 follow/unfollow ([`follow_someone`]) -- reachable through
//!   `nmp-nip02`, an EXPLICIT second dependency (#1707 reversed #1143's
//!   absorption of the follow door into this facade: `nmp` must not know
//!   what a kind:3 contact list or a follow/unfollow edit means). The
//!   `#[cfg(test)]` module below drives it against a real `Engine`, proving
//!   usable, not just nameable, from the two-crate combination.
//! - media composition is deliberately ABSENT from this crate (#1707
//!   reversed #1563's absorption of `nmp-media` into the facade): `nmp` must
//!   not contain any capability's implementation, and the seam itself never
//!   needed the engine. A picture-composing app depends on `nmp-media`
//!   directly, whose own `tests/composition.rs` proves it end to end.
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
    LiveQuery::single(
        Demand::author_outboxes(Filter {
            kinds: Some(std::collections::BTreeSet::from([CALLER_CONTENT_KIND])),
            authors: Some(nmp::Binding::Derived(Box::new(Derived {
                inner: Demand::author_outboxes(Filter {
                    kinds: Some(std::collections::BTreeSet::from([CALLER_INDEX_KIND])),
                    authors: Some(nmp::Binding::Reactive(IdentityField::ActivePubkey)),
                    tags: std::collections::BTreeMap::from([(
                        IndexedTagName::new('d')
                            .expect("'d' is a valid ASCII-letter indexed tag key"),
                        nmp::Binding::Literal(std::collections::BTreeSet::from([
                            "arbitrary-caller-group".to_string(),
                        ])),
                    )]),
                    ..Filter::default()
                })
                .expect("the selection binds `authors`"),
                project: Selector::Tag("p".to_string()),
            }))),
            ..Filter::default()
        })
        .expect("the selection binds `authors`"),
    )
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

/// Names the NIP-22 comment vocabulary and composes its write operation
/// through `nmp-nip22`, an explicit second dependency (#1707): a direct-Rust
/// app reaches exactly what `nmp-ffi`'s `comment_intent` projection reaches,
/// `nmp-nip22` the ONE owner either way. What comes back is an ordinary
/// [`WriteIntent`] (#907), published through the same `Engine::publish`
/// lifecycle as any other write. Uses an external NIP-73 content id so no
/// NIP-01 core kind is baked into this proof. The vocabulary is engine-free
/// and still reads no ambient clock or current account -- it no longer
/// needs an author or an event time to say so, because the engine resolves
/// both at acceptance.
pub fn build_comment_intent(
    guid: &str,
    content: &str,
) -> Result<WriteIntent, nmp_nip22::Nip73Error> {
    let root = nmp_nip22::CommentRoot::External(nmp_nip22::Nip73::podcast_episode(guid)?);
    Ok(nmp_nip22::comment_intent(&root, content.to_string()))
}

/// Names every protocol/content family #1239 once retrofitted onto the
/// facade and #1707 later moved back out to its own crate.
///
/// This is deliberately one function rather than several: every family here
/// is usable in combination, not just individually nameable. `nmp-nip18`/
/// `nmp-nipc7`/`nmp-nip25`/`nmp-content`/`nmp-asset`/`nmp-blossom` are each
/// an explicit dependency line (#1707: none of these needed engine coupling,
/// so nothing forced them into `nmp` as re-export doors in the first place).
/// `nmp-nip29` is the same shape for a different reason: its Group/
/// group-list-write door DID need `nmp`'s engine surface, but that need
/// runs `nmp-nip29 -> nmp`, an ordinary capability-composes-the-engine
/// edge, not the reverse `nmp -> nmp-nip29` a facade feature would be.
///
/// Every door here composes and returns rather than merely being imported, so
/// removing any one import breaks this crate instead of leaving a stale
/// claim in a doc comment. `nip02` is proven separately, by
/// [`follow_someone`] and this crate's `#[cfg(test)]` module: its write door
/// needs a live `Engine`, not a target event, so it does not fit this
/// function's pure-composition shape.
pub fn compose_every_retrofitted_family(target: &Event, source: Option<RelayUrl>) -> Vec<String> {
    // NIP-C7 kind:9 chat, top-level and threaded (`nmp_nipc7`).
    let chat = nmp_nipc7::chat();
    let chat_reply = nmp_nipc7::chat_reply(target);
    // NIP-18 repost, whose whole value is that the caller never picks between
    // kind:6 and kind:16 (`nmp_nip18`).
    let repost = nmp_nip18::repost(target, source.clone());
    // NIP-25 reaction (`nmp_nip25`), wired at birth by #155 and named here so
    // the retrofit and the family that avoided it are proven the same way.
    let reaction = nmp_nip25::react(target, source, nmp_nip25::Reaction::Like);
    // NIP-51 kind:10009: the demand that reads it and the tolerant codec that
    // decodes what came back (`nmp_nip29`).
    let groups_demand: Demand = nmp_nip29::current_account_group_list_demand();
    let groups: nmp_nip29::SimpleGroupsList = nmp_nip29::parse_simple_groups_list_tolerant(target);
    let first_group: Option<&nmp_nip29::SimpleGroupEntry> = groups.items.first();
    // Content parsing (`nmp_content`) -- the door mosaico hand-rolled a
    // `find("nostr:")` scanner for, because it could not reach this one.
    let document: nmp_content::ContentDocument =
        nmp_content::parse_content(&target.content, nmp_content::ContentSyntax::PlainText);
    let references: Vec<&NostrEntity> = document
        .references()
        .into_iter()
        .map(|occurrence: &nmp_content::ReferenceOccurrence| &occurrence.target)
        .collect();
    // Exact-byte identity and the Blossom vocabulary built on it
    // (`nmp_asset`, `nmp_blossom`).
    let digest: nmp_asset::Sha256Hash = nmp_asset::Sha256Hash::of(target.content.as_bytes());
    let verbs = [
        nmp_blossom::BlossomVerb::Upload,
        nmp_blossom::BlossomVerb::Delete,
        nmp_blossom::BlossomVerb::List,
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

/// The demand `nmp_nip02` reads the current account's kind:3 contact list
/// through -- reachable from the explicit `nmp` + `nmp-nip02` pair, the
/// uniform two-crate cost #1707 restored for every capability.
#[must_use]
pub fn follow_demand() -> Demand {
    nmp_nip02::current_account_demand()
}

/// Follows `target` through the ordinary NIP-02 write door. `writes`
/// composes once (typically held for the process lifetime); this function
/// takes it by reference so a caller following many targets does not
/// re-register the capability handle each time.
pub fn follow_someone(
    engine: &Engine,
    writes: &nmp_nip02::FollowWrites,
    target: PublicKey,
) -> Result<ReceiptStream, nmp_nip02::FollowActionFailure> {
    nmp_nip02::set_following(engine, writes, target, nmp_nip02::FollowChange::Follow)
}

/// The unfollow half of [`follow_someone`], proving both directions of
/// [`nmp_nip02::FollowChange`] are reachable from the same two-crate pair.
pub fn unfollow_someone(
    engine: &Engine,
    writes: &nmp_nip02::FollowWrites,
    target: PublicKey,
) -> Result<ReceiptStream, nmp_nip02::FollowActionFailure> {
    nmp_nip02::set_following(engine, writes, target, nmp_nip02::FollowChange::Unfollow)
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

    /// #1707 acceptance proof: a direct-Rust app follows and unfollows
    /// through the explicit `nmp` + `nmp-nip02` pair, and opens the
    /// reactive contact-list demand [`follow_demand`] reads it through --
    /// proving the two-crate combination usable, not just nameable.
    #[test]
    fn follow_and_unfollow_are_usable_from_nmp_and_nmp_nip02() {
        let engine = Engine::new_with_capabilities(
            EngineConfig::default(),
            vec![nmp_nip02::follow_capability()],
        )
        .expect("temporary Redb engine must build");
        engine
            .add_private_key_account(&TEST_SECRET_KEY_BYTES, true)
            .expect("fixed decoded test secret key must validate");
        let writes = nmp_nip02::follow_writes();
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

    /// Routing acceptance proof: a direct-Rust app CHOOSES its author-route
    /// algorithm through the explicit `nmp` + `nmp-outbox` pair, the same
    /// two-crate shape every capability has. `nmp` names no routing
    /// protocol and no algorithm; the provider is a constructor argument,
    /// so choosing a different algorithm changes this crate's manifest and
    /// nothing inside NMP.
    #[test]
    fn outbox_routing_is_chosen_from_nmp_and_nmp_outbox() {
        let indexer: RelayUrl = "wss://indexer.example"
            .parse()
            .expect("fixed indexer URL must parse");
        let engine = Engine::new_with_capabilities_and_routing(
            EngineConfig::default(),
            Vec::new(),
            Some(Box::new(nmp_outbox::Nip65Outbox::new([indexer]))),
        )
        .expect("an engine with a chosen routing algorithm must build");

        // And the same door takes NO algorithm at all, which is a supported
        // choice rather than a missing feature: operator lanes and explicit
        // routes still carry everything they carry.
        let providerless =
            Engine::new_with_capabilities_and_routing(EngineConfig::default(), Vec::new(), None)
                .expect("an engine that discovers no routes must build");

        engine.shutdown();
        providerless.shutdown();
    }
}
