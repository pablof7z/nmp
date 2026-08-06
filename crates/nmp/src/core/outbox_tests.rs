//! Falsifiers for the built-in outbox resolver: its three sources, its
//! per-recipient coverage top-up, and its three-valued settlement.
//!
//! These own the executable half of `features/routing/outbox-default-fan-out`,
//! `outbox-fallback-coverage` and `outbox-recipients-and-settlement`. Each
//! test asserts the WHOLE answer -- the relay set, the waiting set, and
//! `complete` -- because every one of those scenarios is a claim about what
//! is absent from the route as much as what is present, and a containment
//! assertion cannot fail for a resolver that over-routes.

use super::*;

#[cfg(test)]
mod outbox_resolver_tests {
    use super::*;

    use crate::core::write::RouteAnswer;
    use nmp_router::FixtureRoutingFacts;
    use nmp_store::MemoryStore;
    use nostr::{Keys, Kind};

    fn relay(name: &str) -> RelayUrl {
        RelayUrl::parse(&format!("wss://{name}.example")).expect("fixture relay url")
    }

    fn relays<const N: usize>(names: [&str; N]) -> BTreeSet<RelayUrl> {
        names.into_iter().map(relay).collect()
    }

    /// The frozen note the resolver reads: an author and the public keys it
    /// `p`-tags. Nothing else about the event reaches `resolve_routes`.
    fn note(author: PublicKey, recipients: &[PublicKey]) -> SignedEvent {
        let created_at = Timestamp::from(1_700_000_000);
        let kind = Kind::TextNote;
        let tags = nostr::Tags::from_list(
            recipients
                .iter()
                .map(|recipient| nostr::Tag::public_key(*recipient))
                .collect(),
        );
        let content = "outbox".to_string();
        SignedEvent::new(
            EventId::new(&author, &created_at, &kind, &tags, &content),
            author,
            created_at,
            kind,
            tags,
            content,
            nmp_store::sentinel_signature(),
        )
    }

    /// Resolve `event` under `Auto` against exactly these facts.
    fn route(facts: FixtureRoutingFacts, event: &SignedEvent) -> RouteAnswer {
        EngineCore::new_with_fixture_routing_facts(MemoryStore::new(), facts, 10)
            .resolve_routes(&WriteRouting::Auto, event)
    }

    // ---- the three sources ------------------------------------------------

    /// App relays are source 2 unconditionally, not a top-up for a thin
    /// source 1: an event addressed to nobody, whose author has two healthy
    /// write relays, still reaches both sets.
    #[test]
    fn an_unaddressed_note_still_unions_the_author_outbox_with_the_app_relays() {
        let author = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_app(relays(["app-indexer-1", "app-indexer-2"])),
            &note(author, &[]),
        );

        assert_eq!(
            answer.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "app-indexer-1",
                "app-indexer-2"
            ]),
            "the operator's relays are added to a perfectly healthy author outbox, not instead of it: {answer:?}"
        );
        assert!(
            answer.complete && answer.author_route_needs.is_empty(),
            "every contributor answered, so there is nothing left to learn: {answer:?}"
        );
    }

    /// An author fact owns two directional sets and the AUTHOR role reads the
    /// outbound one. Routing a note to where its author collects mail tells
    /// nobody anything.
    #[test]
    fn the_author_is_reached_at_their_outbound_half_and_never_their_inbound_half() {
        let author = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new().with_author_routes(
                author,
                relays(["author-write-1", "author-write-2"]),
                relays(["author-read-only"]),
            ),
            &note(author, &[]),
        );

        assert_eq!(
            answer.relays,
            relays(["author-write-1", "author-write-2"]),
            "the author's inbound half is not a destination for their own write: {answer:?}"
        );
        assert!(
            answer.complete,
            "a present author fact is settled: {answer:?}"
        );
    }

    /// The mirror image, and the load-bearing distinction of the default: a
    /// RECIPIENT is reached at their inbound relays. Delivering to the relays
    /// someone publishes to is a message they will never read, and the two
    /// sets are routinely disjoint.
    #[test]
    fn a_recipient_is_reached_at_their_inbound_half_and_never_their_outbound_half() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_author_routes(bob, relays(["bob-outbox"]), relays(["bob-inbox"])),
            &note(author, &[bob]),
        );

        assert_eq!(
            answer.relays,
            relays(["author-write-1", "author-write-2", "bob-inbox"]),
            "a recipient contributes their inbox and only their inbox: {answer:?}"
        );
    }

    /// The fan-out is per recipient: three addressees, three inboxes, all of
    /// them.
    #[test]
    fn every_p_tagged_recipient_contributes_their_own_inbox() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();
        let carol = Keys::generate().public_key();
        let dave = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_inbound_routes(bob, relays(["bob-inbox"]))
                .with_inbound_routes(carol, relays(["carol-inbox"]))
                .with_inbound_routes(dave, relays(["dave-inbox"])),
            &note(author, &[bob, carol, dave]),
        );

        assert_eq!(
            answer.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "bob-inbox",
                "carol-inbox",
                "dave-inbox"
            ]),
            "each addressee contributes their own inbox, not the first one and not a sample: {answer:?}"
        );
    }

    /// Composition: there is no precedence between the three sources and no
    /// "most specific wins" -- the answer is a set union.
    #[test]
    fn the_three_outbox_sources_compose_by_union_with_no_precedence() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_app(relays(["app-indexer-1", "app-indexer-2"]))
                .with_inbound_routes(bob, relays(["bob-inbox"])),
            &note(author, &[bob]),
        );

        assert_eq!(
            answer.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "app-indexer-1",
                "app-indexer-2",
                "bob-inbox"
            ]),
            "an author, an operator and an addressee all reach their own relays: {answer:?}"
        );
        assert!(answer.complete, "{answer:?}");
    }

    /// The resolver reads engine-owned facts about the author, the operator
    /// and the p-tagged recipients, and nothing else. A stranger the
    /// directory happens to know about is not a source.
    #[test]
    fn the_outbox_answer_never_names_a_relay_outside_its_three_sources() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();
        let stranger = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_app(relays(["app-indexer-1"]))
                .with_inbound_routes(bob, relays(["bob-inbox"]))
                .with_author_routes(
                    stranger,
                    relays(["stranger-outbox"]),
                    relays(["stranger-inbox"]),
                ),
            &note(author, &[bob]),
        );

        assert_eq!(
            answer.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "app-indexer-1",
                "bob-inbox"
            ]),
            "an author nobody named is not a source, however warm the directory is on them: {answer:?}"
        );
    }

    // ---- the operator fallback top-up -------------------------------------

    /// The motivating case: a reply to someone whose relay list names exactly
    /// one relay. One relay is one point of failure for the whole reply, so
    /// the operator fallbacks top THAT PERSON up.
    #[test]
    fn a_recipient_below_the_coverage_minimum_arms_the_operator_fallback() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_fallback(relays(["fallback-1", "fallback-2"]))
                .with_inbound_routes(bob, relays(["bob-only-inbox"])),
            &note(author, &[bob]),
        );

        assert_eq!(
            answer.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "bob-only-inbox",
                "fallback-1",
                "fallback-2"
            ]),
            "a single-relay addressee is topped up to more than one point of failure: {answer:?}"
        );
        assert!(answer.complete, "{answer:?}");
    }

    /// Zero is below two. A settled absence contributes no inbox of its own,
    /// and the fallbacks are the only chance the note has of reaching them --
    /// without keeping the routing open, because absence is settled
    /// knowledge.
    #[test]
    fn a_settled_absent_recipient_is_below_coverage_and_arms_the_fallback() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_fallback(relays(["fallback-1", "fallback-2"]))
                .with_author_absent(bob),
            &note(author, &[bob]),
        );

        assert_eq!(
            answer.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "fallback-1",
                "fallback-2"
            ]),
            "a recipient with no reachable inbox is topped up like any other short one: {answer:?}"
        );
        assert!(
            answer.complete && answer.author_route_needs.is_empty(),
            "a settled absence is an answer, so it must not hold the route open: {answer:?}"
        );
    }

    /// The read path's own suppression rule, transplanted: app relays
    /// suppress fallback entirely WITHOUT themselves counting toward the
    /// coverage minimum. Bob stays on one inbox and the operator's choice
    /// stands.
    #[test]
    fn an_app_relay_suppresses_the_fallback_without_itself_counting_as_coverage() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_app(relays(["app-indexer"]))
                .with_operator_fallback(relays(["fallback-1", "fallback-2"]))
                .with_inbound_routes(bob, relays(["bob-only-inbox"])),
            &note(author, &[bob]),
        );

        assert_eq!(
            answer.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "bob-only-inbox",
                "app-indexer"
            ]),
            "an operator who configured app relays already answered this question: {answer:?}"
        );
    }

    /// Suppression keys on the PRESENCE of an app relay set, not on its size
    /// and not on whether it restored coverage.
    #[test]
    fn one_app_relay_suppresses_the_fallback_however_thin_it_is() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_app(relays(["app-indexer"]))
                .with_operator_fallback(relays(["fallback-1", "fallback-2"]))
                .with_author_absent(bob),
            &note(author, &[bob]),
        );

        assert_eq!(
            answer.relays,
            relays(["author-write-1", "author-write-2", "app-indexer"]),
            "one app relay suppresses the top-up without satisfying it: {answer:?}"
        );
    }

    /// Two is the minimum and two is enough: an addressee already at coverage
    /// gets no fallback, or every reply forever would widen by the operator's
    /// whole set.
    #[test]
    fn a_recipient_already_at_the_coverage_minimum_gets_no_fallback() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_fallback(relays(["fallback-1", "fallback-2"]))
                .with_inbound_routes(bob, relays(["bob-inbox-1", "bob-inbox-2"])),
            &note(author, &[bob]),
        );

        assert_eq!(
            answer.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "bob-inbox-1",
                "bob-inbox-2"
            ]),
            "a well-covered addressee has nothing for the top-up to fix: {answer:?}"
        );
    }

    /// "Per recipient" is only observable through a pair. An implementation
    /// that averaged coverage across the recipient set, or that asked "does
    /// this EVENT have enough relays" rather than "does this PERSON", passes
    /// every single-recipient case above and fails here.
    #[test]
    fn coverage_is_decided_per_recipient_not_across_the_recipient_set() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();
        let carol = Keys::generate().public_key();
        let facts = || {
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_fallback(relays(["fallback-1", "fallback-2"]))
                .with_inbound_routes(bob, relays(["bob-only-inbox"]))
                .with_inbound_routes(carol, relays(["carol-inbox-1", "carol-inbox-2"]))
        };

        let both = route(facts(), &note(author, &[bob, carol]));
        assert_eq!(
            both.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "bob-only-inbox",
                "carol-inbox-1",
                "carol-inbox-2",
                "fallback-1",
                "fallback-2"
            ]),
            "Carol being well covered says nothing about Bob: {both:?}"
        );

        let covered_alone = route(facts(), &note(author, &[carol]));
        assert_eq!(
            covered_alone.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "carol-inbox-1",
                "carol-inbox-2"
            ]),
            "an amply covered addressee alone arms nothing, or the top-up is just \"any recipient exists\": {covered_alone:?}"
        );
    }

    /// A write already fans out to EVERY write relay its author has. One
    /// write relay of my own is a fact about where I publish, not a coverage
    /// deficit -- the minimum is about reaching the ADDRESSEE.
    #[test]
    fn the_authors_own_thin_outbox_is_never_a_coverage_deficit() {
        let author = Keys::generate().public_key();
        let carol = Keys::generate().public_key();
        let facts = || {
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1"]))
                .with_operator_fallback(relays(["fallback-1", "fallback-2"]))
                .with_inbound_routes(carol, relays(["carol-inbox-1", "carol-inbox-2"]))
        };

        let unaddressed = route(facts(), &note(author, &[]));
        assert_eq!(
            unaddressed.relays,
            relays(["author-write-1"]),
            "a single-relay author publishes where they publish: {unaddressed:?}"
        );
        assert!(unaddressed.complete, "{unaddressed:?}");

        let addressed = route(facts(), &note(author, &[carol]));
        assert_eq!(
            addressed.relays,
            relays(["author-write-1", "carol-inbox-1", "carol-inbox-2"]),
            "a thin author beside a covered addressee still arms nothing: {addressed:?}"
        );
    }

    /// Nothing about the top-up is required for routing to succeed: with no
    /// fallbacks configured the route is what the three sources yielded, and
    /// it completes.
    #[test]
    fn a_thin_recipient_with_no_configured_fallback_still_completes() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_inbound_routes(bob, relays(["bob-only-inbox"])),
            &note(author, &[bob]),
        );

        assert_eq!(
            answer.relays,
            relays(["author-write-1", "author-write-2", "bob-only-inbox"]),
            "{answer:?}"
        );
        assert!(
            answer.complete,
            "below coverage with nothing to top up with is not a failure: {answer:?}"
        );
    }

    // ---- settlement: what finishes an outbox and what keeps it open -------

    /// The owner's worked example: three addressees, one relay list between
    /// them, and the obligation retires with two of the three contributing
    /// nothing at all.
    #[test]
    fn three_recipients_with_one_relay_list_between_them_retire_the_obligation() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();
        let carol = Keys::generate().public_key();
        let dave = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_app(relays(["app-indexer"]))
                .with_inbound_routes(bob, relays(["bob-inbox"]))
                .with_author_absent(carol)
                .with_author_absent(dave),
            &note(author, &[bob, carol, dave]),
        );

        assert_eq!(
            answer.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "app-indexer",
                "bob-inbox"
            ]),
            "{answer:?}"
        );
        assert!(
            answer.complete && answer.author_route_needs.is_empty(),
            "there is nothing left to learn, so the obligation retires: {answer:?}"
        );
    }

    /// The unit version: a settled absence contributes nothing AND blocks
    /// nothing. That is what makes it categorically different from an
    /// unlooked-up recipient, which looks identical in the relay set.
    #[test]
    fn a_settled_absent_recipient_adds_no_relay_and_delays_nothing() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_app(relays(["app-indexer"]))
                .with_author_absent(bob),
            &note(author, &[bob]),
        );

        assert_eq!(
            answer.relays,
            relays(["author-write-1", "author-write-2", "app-indexer"]),
            "{answer:?}"
        );
        assert!(
            answer.complete && answer.author_route_needs.is_empty(),
            "settled absence is an answer, not a wait: {answer:?}"
        );
    }

    /// The distinction that earns the three-valued model: an unlooked-up
    /// recipient is UNKNOWN, not empty. The relays already known are used
    /// now -- incompleteness delays the finish, not the delivery.
    #[test]
    fn an_unlooked_up_recipient_keeps_the_answer_open_while_known_relays_are_used_now() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();
        let carol = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_app(relays(["app-indexer"]))
                .with_inbound_routes(bob, relays(["bob-inbox"])),
            &note(author, &[bob, carol]),
        );

        assert_eq!(
            answer.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "app-indexer",
                "bob-inbox"
            ]),
            "what is known is routed immediately: {answer:?}"
        );
        assert!(
            !answer.complete,
            "an unknown recipient is not an answer: {answer:?}"
        );
        assert_eq!(
            answer.author_route_needs,
            BTreeSet::from([carol]),
            "the open answer names exactly who it is still waiting on: {answer:?}"
        );
    }

    /// When the unknown settles POSITIVELY the route grows and finishes. The
    /// destination set moves and `complete` flips in the same resolution.
    #[test]
    fn a_recipients_arriving_relay_list_completes_the_route_it_was_holding_open() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();
        let carol = Keys::generate().public_key();
        let known = || {
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_app(relays(["app-indexer"]))
                .with_inbound_routes(bob, relays(["bob-inbox"]))
        };
        let event = note(author, &[bob, carol]);

        let before = route(known(), &event);
        assert!(!before.complete, "{before:?}");

        let after = route(
            known().with_inbound_routes(carol, relays(["carol-inbox"])),
            &event,
        );
        assert_eq!(
            after.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "app-indexer",
                "bob-inbox",
                "carol-inbox"
            ]),
            "the late inbox joins the same route: {after:?}"
        );
        assert!(
            after.complete && after.author_route_needs.is_empty(),
            "with the last unknown settled the obligation retires: {after:?}"
        );
    }

    /// The other settlement outcome, and the one that makes retirement
    /// reachable at all. If only the positive outcome were implemented, every
    /// note to someone without a relay list would be routed forever.
    #[test]
    fn an_unknown_that_settles_absent_finishes_the_route_without_adding_a_relay() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();
        let carol = Keys::generate().public_key();
        let known = || {
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_app(relays(["app-indexer"]))
                .with_inbound_routes(bob, relays(["bob-inbox"]))
        };
        let event = note(author, &[bob, carol]);

        let before = route(known(), &event);
        let after = route(known().with_author_absent(carol), &event);

        assert_eq!(
            before.relays, after.relays,
            "an absence names no relay, so the destination set cannot move: {before:?} -> {after:?}"
        );
        assert_eq!(
            (before.complete, after.complete),
            (false, true),
            "what changed is that there is nothing left to wait for: {before:?} -> {after:?}"
        );
    }

    /// Completion is a property of the WHOLE recipient set, not a majority of
    /// it: four of five answered, and the fifth alone holds it open.
    #[test]
    fn one_unlooked_up_recipient_among_settled_ones_keeps_the_answer_open() {
        let author = Keys::generate().public_key();
        let bob = Keys::generate().public_key();
        let carol = Keys::generate().public_key();
        let dave = Keys::generate().public_key();
        let erin = Keys::generate().public_key();
        let frank = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"]))
                .with_operator_app(relays(["app-indexer"]))
                .with_inbound_routes(bob, relays(["bob-inbox"]))
                .with_inbound_routes(carol, relays(["carol-inbox"]))
                .with_author_absent(dave)
                .with_author_absent(erin),
            &note(author, &[bob, carol, dave, erin, frank]),
        );

        assert_eq!(
            answer.relays,
            relays([
                "author-write-1",
                "author-write-2",
                "app-indexer",
                "bob-inbox",
                "carol-inbox"
            ]),
            "the route it already has keeps being used meanwhile: {answer:?}"
        );
        assert!(!answer.complete, "{answer:?}");
        assert_eq!(
            answer.author_route_needs,
            BTreeSet::from([frank]),
            "one unanswered recipient, named: {answer:?}"
        );
    }

    /// A published relay list that names no WRITE relay is an ANSWER: the
    /// author said, on the record, "I write nowhere in particular". The route
    /// completes on the app relays alone rather than parking on a list that
    /// has already arrived.
    #[test]
    fn an_author_who_declared_no_write_relays_is_settled_not_unknown() {
        let author = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_author_routes(author, [], [])
                .with_operator_app(relays(["app-indexer"])),
            &note(author, &[]),
        );

        assert_eq!(answer.relays, relays(["app-indexer"]), "{answer:?}");
        assert!(
            answer.complete && answer.author_route_needs.is_empty(),
            "a present-but-empty outbound half is knowledge, not ignorance: {answer:?}"
        );
    }

    /// The shape this turns up as in the wild: a list with entries, all of
    /// them read-marked, so the WRITE set is empty while the list plainly
    /// exists. Same conclusion, and the inbound entries are still not
    /// destinations for the author's own write.
    #[test]
    fn an_author_whose_entries_are_all_inbound_has_a_settled_empty_outbox() {
        let author = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new()
                .with_author_routes(author, [], relays(["author-read-only"]))
                .with_operator_app(relays(["app-indexer"])),
            &note(author, &[]),
        );

        assert_eq!(
            answer.relays,
            relays(["app-indexer"]),
            "an all-inbound list is a settled empty outbox, and its entries are not destinations: {answer:?}"
        );
        assert!(answer.complete, "{answer:?}");
    }

    /// The cold start. Not knowing yet is the normal INITIAL state of this
    /// resolver, so it parks -- with whatever the operator already gave it --
    /// instead of erroring and killing a signed, journalled, durable event.
    #[test]
    fn an_unlooked_up_author_parks_the_route_and_keeps_the_operator_relay_it_has() {
        let author = Keys::generate().public_key();

        let answer = route(
            FixtureRoutingFacts::new().with_operator_app(relays(["app-indexer"])),
            &note(author, &[]),
        );

        assert_eq!(
            answer.relays,
            relays(["app-indexer"]),
            "the operator relay is used now, not held back: {answer:?}"
        );
        assert!(
            !answer.complete,
            "a young directory must not be mistaken for an exhausted one: {answer:?}"
        );
        assert_eq!(
            answer.author_route_needs,
            BTreeSet::from([author]),
            "the park names the author whose list would unpark it: {answer:?}"
        );
    }

    /// The author arm of the wake: the same event, resolved again once the
    /// list is in, finishes.
    #[test]
    fn an_authors_arriving_relay_list_completes_the_route_that_waited_on_it() {
        let author = Keys::generate().public_key();
        let event = note(author, &[]);

        let before = route(
            FixtureRoutingFacts::new().with_operator_app(relays(["app-indexer"])),
            &event,
        );
        assert!(!before.complete, "{before:?}");

        let after = route(
            FixtureRoutingFacts::new()
                .with_operator_app(relays(["app-indexer"]))
                .with_outbound_routes(author, relays(["author-write-1", "author-write-2"])),
            &event,
        );

        assert_eq!(
            after.relays,
            relays(["author-write-1", "author-write-2", "app-indexer"]),
            "{after:?}"
        );
        assert!(
            after.complete && after.author_route_needs.is_empty(),
            "the answer that was waited on retires the obligation: {after:?}"
        );
    }
}
