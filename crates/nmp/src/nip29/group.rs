//! [`Group`] -- one group id within a [`RelayScope`](super::RelayScope)
//! (#1033).
//!
//! A group is an IDENTITY, not a subscription: the hosts its scope named plus
//! one group id, and nothing else. Constructing one contacts nothing. The
//! same value serves every read and every write for a room's whole lifetime.
//!
//! It retains both privately. There is no host accessor, no id accessor, and
//! no method that takes a per-call host, route, group id or raw `h` row --
//! that is the mechanism, not a convention: an app cannot compose an event
//! under one group and route it as though it came from another, because
//! there is no spelling for saying so.
//!
//! Reads of a group's CONTENT mint a [`LiveQuery`] the ordinary observe door
//! takes. [`Group::observe`] reads NIP-29's own relay-signed records, and it
//! is a projection over that same door -- it opens the engine's own
//! subscription and folds the deltas an app would otherwise fold by hand,
//! exactly as `nmp_nip02`'s follow observation does. What stays absent is a
//! second read LIFECYCLE: no socket, no retry, no group-shaped cancellation,
//! which is the read-side shape of the thing #838 deleted on the write side.
//!
//! # The write half MINTS a [`WriteIntent`], and that is the whole product
//!
//! [`Group::intent`] and [`Group::signed_intent`] are the write door
//! (#1242). They hand back the ordinary opaque [`WriteIntent`] the ONE
//! publish door takes -- `h` row appended before signing, route minted from
//! the retained scope, author frozen -- and publish nothing.
//! [`Group::publish`] and every named operation are exactly one of those two
//! calls plus [`Engine::publish_tracked`]; there is no second
//! contextualization, no group-shaped receipt and no group-shaped retry. An
//! app whose write architecture mints intents in one stage and submits them
//! in another therefore uses the SAME door as an app that publishes inline,
//! and neither one ever spells a relay or an `h` row.
//!
//! Because the minted intent is an ordinary value with public fields, an app
//! holding one can read the route the group chose and, from the payload's own
//! `h` row, the group id. That is a real widening of what a `Group` yields
//! and it is stated rather than hidden: the alternative -- a group-shaped
//! intent noun that only a group-shaped publish door accepts -- is a second
//! write lifecycle, which is the thing this module exists not to have. What
//! the non-readback still buys is unchanged for every layer that holds a
//! `Group` and does not call the write door: no accessor reconstructs the
//! authority.
//!
//! It is also how a group write becomes crash-recoverable (#1244).
//! [`WriteIntent::correlation`] is caller-minted and caller-persisted, so the
//! group door neither takes one nor invents one: the app stamps its own token
//! on the minted intent and hands it to [`Engine::publish_tracked`], and
//! `reattach_by_correlation` then recovers that write like any other. The
//! inline doors return the ordinary [`ReceiptStream`] -- store-issued
//! [`ReceiptId`](crate::ReceiptId) included -- for the same reason: a group
//! write is a tracked write and always was.

use std::collections::BTreeSet;

use nmp_grammar::{EventBuilder, Filter, Identity, WriteIntent, WritePayload, WriteRouting};
use nostr::{Event, EventId, PublicKey, RelayUrl};

use super::read::{self, GroupReadError};
use super::records::{GroupObservation, GroupObserveError};
use crate::engine::Engine;
use crate::error::EngineError;
use crate::runtime::ReceiptStream;
use crate::LiveQuery;
use nmp_nip29::GroupContextError;
use nmp_nip29::GroupRecord;

/// Why a group publication never reached the publish door, or what the door
/// said when it did.
///
/// The two halves are kept apart because they are different kinds of fact: a
/// [`Self::Context`] is a CALLER error decided before anything was accepted --
/// no signature, no journal row, no receipt -- while a [`Self::Engine`] is the
/// ordinary publish door refusing the intent. Neither is a relay rejection; a
/// host that refuses the event does so on the receipt stream, like every other
/// write. A publication accepted by one host and rejected by another is
/// therefore two ordinary per-relay facts on one receipt, not an error here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupPublishError {
    /// The draft or signed event could not be contextualized for this group.
    Context(GroupContextError),
    /// The publish door refused the intent.
    Engine(EngineError),
}

impl From<GroupContextError> for GroupPublishError {
    fn from(error: GroupContextError) -> Self {
        Self::Context(error)
    }
}

impl From<EngineError> for GroupPublishError {
    fn from(error: EngineError) -> Self {
        Self::Engine(error)
    }
}

impl std::fmt::Display for GroupPublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Context(error) => write!(f, "{error}"),
            Self::Engine(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for GroupPublishError {}

/// One NIP-29 group, on the relays its scope named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    hosts: BTreeSet<RelayUrl>,
    id: String,
}

impl Group {
    pub(super) fn new(hosts: BTreeSet<RelayUrl>, id: String) -> Self {
        debug_assert!(!hosts.is_empty(), "a scope proves its host set is nonempty");
        Self { hosts, id }
    }

    /// Mint the read declaration for an APP-SUPPLIED selection.
    ///
    /// The group contributes exactly two things: one complete branch per host
    /// and the `#h` scoping. Which kinds live in the group is the app's to say
    /// -- a fixed content catalogue here is precisely what #838 removed. A
    /// selection that already constrains `#h` is REFUSED, because the retained
    /// group id is the sole semantic source of that row.
    ///
    /// Hand the result to the one read door: `engine.observe(query, None)`.
    pub fn read(&self, selection: Filter) -> Result<LiveQuery, GroupReadError> {
        read::one_live_query(self.read_branches(selection)?)
    }

    /// One complete read branch per host, in canonical host order. Split out
    /// for the same reason as
    /// [`RelayScope::records_branches`](super::RelayScope::records_branches):
    /// the per-branch scoping property must be assertable for a MULTI-host
    /// group independently of how branches are aggregated.
    pub(crate) fn read_branches(
        &self,
        selection: Filter,
    ) -> Result<Vec<nmp_grammar::Demand>, GroupContextError> {
        self.hosts
            .iter()
            .map(|host| nmp_nip29::group_demand_at(host, &self.id, selection.clone()))
            .collect()
    }

    /// Watch this group's own relay-signed records.
    ///
    /// The five-line path: no predicate, no collection, no id lookup. Each
    /// delivery carries exactly one [`GroupSnapshot`] -- this group's -- from
    /// the first delivery onward, including before any record has arrived, so
    /// there is always something to render.
    ///
    /// This is not a second read door. It opens the ONE ordinary
    /// `Engine::observe_async` subscription over the ONE ordinary
    /// [`LiveQuery`] the group's hosts declare, and folds the deltas the app
    /// would otherwise fold by hand -- the same relationship
    /// `nmp_nip02`'s follow observation has to the same door. The group owns
    /// no socket, no retry and no cancellation semantics of its own.
    ///
    /// ```text
    /// let group = nip29::group([host], room_id)?;
    /// let watching = group.observe(&engine, [GroupRecord::Metadata, GroupRecord::Members])?;
    /// while let Some(snapshots) = watching.next().await? {
    ///     let room = &snapshots[0];
    /// }
    /// ```
    pub fn observe(
        &self,
        engine: &Engine,
        records: impl IntoIterator<Item = GroupRecord>,
    ) -> Result<GroupObservation, GroupObserveError> {
        let records: BTreeSet<GroupRecord> = records.into_iter().collect();
        if records.is_empty() {
            return Err(GroupObserveError::NoRecordSelected);
        }
        let this_id = nmp_grammar::Binding::Literal(BTreeSet::from([self.id.clone()]));
        let predicate: super::GroupPredicate = super::any_of(this_id).into();
        let branches = self
            .hosts
            .iter()
            .map(|host| nmp_nip29::group_records_at(host, &records, predicate.lower_at(host), None))
            .collect();
        super::records::observe(
            engine,
            self.hosts.clone(),
            BTreeSet::from([self.id.clone()]),
            branches,
        )
    }

    /// Ask whether an already-signed event belongs to this group, without
    /// building a write out of it.
    pub fn validate_context(&self, event: &Event) -> Result<(), GroupContextError> {
        nmp_nip29::validate_context(&self.id, event)
    }

    /// Mint the group-contextualized [`WriteIntent`] for an unsigned draft,
    /// as `author`, and publish NOTHING (#1242).
    ///
    /// This is the write door. The group appends exactly one
    /// `["h", group_id]` row BEFORE the stamp/sign step, so the context tag
    /// is inside the bytes that get signed, and mints
    /// [`WriteRouting::Explicit`] over every host in the scope. The refusals
    /// -- a caller-supplied `h`, a caller-supplied timeline -- are decided
    /// HERE, before any intent exists, which is where a caller error belongs.
    ///
    /// `author` is an exact decoded [`PublicKey`], never a reactive selector:
    /// a semantic group write freezes who is writing at composition time
    /// rather than resolving it later against whoever happens to be active
    /// (#878). Reactive identity remains entirely valid on the READ side.
    ///
    /// The returned intent's [`correlation`](WriteIntent::correlation) is
    /// `None`, and stamping one is the caller's -- an app that persists a
    /// token before writing sets it here and recovers the write after a crash
    /// with `reattach_by_correlation` (#1244). Everything else about the
    /// intent is already decided; an app that overwrites `routing` or
    /// `payload` has left the door, and there is no reason to.
    ///
    /// Kind-blind: no kind is privileged, refused, or read.
    ///
    /// ```text
    /// let mut intent = group.intent(author, EventBuilder::new(Kind::from(9u16)))?;
    /// intent.correlation = Some(token);
    /// let receipt = engine.publish_tracked(intent)?;
    /// ```
    pub fn intent(
        &self,
        author: PublicKey,
        builder: EventBuilder,
    ) -> Result<WriteIntent, GroupPublishError> {
        let contextualized = nmp_nip29::contextualize(&self.id, builder)?;
        Ok(self.mint(
            WritePayload::Event(contextualized),
            Identity::Explicit(author),
        ))
    }

    /// Mint the group-contextualized [`WriteIntent`] for an ALREADY-SIGNED
    /// event, and publish nothing (#1242).
    ///
    /// The `h` it already carries is VALIDATED, never appended: appending
    /// would change the bytes and therefore the `EventId` the caller already
    /// has. A missing, wrong or duplicated `h` is a typed refusal, not a
    /// repair and not a re-sign.
    ///
    /// There is no author argument. A signed event already fixed its author;
    /// accepting a second selector would let the two disagree.
    pub fn signed_intent(&self, event: Event) -> Result<WriteIntent, GroupPublishError> {
        nmp_nip29::validate_context(&self.id, &event)?;
        let author = event.pubkey;
        Ok(self.mint(WritePayload::Signed(event), Identity::Explicit(author)))
    }

    /// [`Self::intent`] handed straight to the one publish door -- the
    /// inline spelling, for an app that has no separate submit stage.
    ///
    /// Identical in every respect to minting and publishing by hand, because
    /// that is literally its body. The returned [`ReceiptStream`] is the
    /// ordinary one every other write returns, store-issued
    /// [`ReceiptId`](crate::ReceiptId) included.
    pub fn publish(
        &self,
        engine: &Engine,
        author: PublicKey,
        builder: EventBuilder,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.through_the_one_door(engine, self.intent(author, builder)?)
    }

    /// [`Self::signed_intent`] handed straight to the one publish door.
    pub fn publish_signed(
        &self,
        engine: &Engine,
        event: Event,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.through_the_one_door(engine, self.signed_intent(event)?)
    }

    /// kind:9021 -- ask to join. Publishable with no subscription at all:
    /// writing into a group you cannot read yet is the case this door exists
    /// to support.
    pub fn join_request(
        &self,
        engine: &Engine,
        author: PublicKey,
        invite_code: Option<&str>,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::join_request(invite_code))
    }

    /// kind:9022 -- leave.
    pub fn leave_request(
        &self,
        engine: &Engine,
        author: PublicKey,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::leave_request())
    }

    /// kind:9000 -- add a member, optionally with a role.
    pub fn add_user(
        &self,
        engine: &Engine,
        author: PublicKey,
        pubkey: PublicKey,
        role: Option<&str>,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::add_user(pubkey, role))
    }

    /// kind:9001 -- remove a member.
    pub fn remove_user(
        &self,
        engine: &Engine,
        author: PublicKey,
        pubkey: PublicKey,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::remove_user(pubkey))
    }

    /// kind:9002 -- set the group's display fields. An omitted field emits no
    /// tag at all, so it is left untouched rather than cleared.
    pub fn edit_metadata(
        &self,
        engine: &Engine,
        author: PublicKey,
        name: Option<&str>,
        about: Option<&str>,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::edit_metadata(name, about))
    }

    /// kind:9005 -- delete one group-hosted event.
    pub fn delete_event(
        &self,
        engine: &Engine,
        author: PublicKey,
        event_id: EventId,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::delete_event(event_id))
    }

    /// kind:9007 -- create the group at its hosts.
    pub fn create_group(
        &self,
        engine: &Engine,
        author: PublicKey,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::create_group())
    }

    /// kind:9008 -- delete the group from its hosts.
    pub fn delete_group(
        &self,
        engine: &Engine,
        author: PublicKey,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::delete_group())
    }

    /// kind:9009 -- mint an invite code redeemable by
    /// [`join_request`](Self::join_request).
    pub fn create_invite(
        &self,
        engine: &Engine,
        author: PublicKey,
        code: &str,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::create_invite(code))
    }

    /// The one shape a group write has. `Explicit(every host)` is minted
    /// HERE, from the scope the app named once: no caller supplies it and no
    /// engine derives it. The `h` tag carries the GROUP ID, never a relay, so
    /// the hosts are not derivable from the event and no resolver could ever
    /// compute them.
    ///
    /// `correlation: None` is not a decision this door is entitled to make
    /// differently. A correlation token is minted and persisted by the app
    /// BEFORE it writes -- that is the entire point of it (#591) -- so the
    /// only honest value here is the absence, and [`Self::intent`]'s caller
    /// is the one that can fill it in.
    fn mint(&self, payload: WritePayload, identity: Identity) -> WriteIntent {
        WriteIntent {
            payload,
            routing: WriteRouting::Explicit(self.hosts.iter().cloned().collect()),
            identity,
            correlation: None,
        }
    }

    /// The whole engine-facing body of this type: hand a group-minted intent
    /// to the one publish door. Named so a reader can see there is exactly
    /// one, and so a second write lifecycle could not be added without
    /// deleting this line.
    ///
    /// [`Engine::publish_tracked`] rather than [`Engine::publish`], because
    /// the two are the same door -- `publish` IS `publish_tracked` with the
    /// receipt id thrown away -- and throwing it away is what made a group
    /// write the one write an app could not reattach after a crash (#1244).
    fn through_the_one_door(
        &self,
        engine: &Engine,
        intent: WriteIntent,
    ) -> Result<ReceiptStream, GroupPublishError> {
        engine
            .publish_tracked(intent)
            .map_err(GroupPublishError::Engine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nip29;
    use nostr::{Keys, Kind, Tag, Timestamp, UnsignedEvent};

    const GROUP: &str = "photographers";

    fn host(n: u16) -> RelayUrl {
        RelayUrl::parse(&format!("wss://host-{n}.example.com")).expect("a well-formed host")
    }

    fn engine() -> Engine {
        Engine::new(crate::config::EngineConfig::default()).expect("an in-memory engine builds")
    }

    fn author() -> PublicKey {
        Keys::generate().public_key()
    }

    fn group(hosts: impl IntoIterator<Item = RelayUrl>) -> Group {
        nip29::on(hosts).expect("a nonempty scope").group(GROUP)
    }

    fn routed(intent: &WriteIntent) -> Vec<RelayUrl> {
        match &intent.routing {
            WriteRouting::Explicit(relays) => relays.clone(),
            WriteRouting::Auto => panic!("a group write is never Auto"),
        }
    }

    /// One event, correctly contextualized for `GROUP` and signed by
    /// `signer` -- the fixture PRESIGNEDPUBLICATION-001 and -006 both need to
    /// exercise a genuine pre-signed event rather than a freshly minted draft.
    fn signed_event(signer: &Keys) -> Event {
        UnsignedEvent::new(
            signer.public_key(),
            Timestamp::from(1_700_000_000u64),
            Kind::from(9u16),
            vec![Tag::parse(["h", GROUP]).unwrap()],
            "first light".to_string(),
        )
        .sign_with_keys(signer)
        .expect("fixture keys sign cleanly")
    }

    /// The multi-relay write contract: EVERY host in the scope, and only
    /// those, in canonical order -- not one host, not a fallback, not Auto.
    #[test]
    fn a_group_write_routes_explicitly_to_every_host_in_the_scope() {
        let group = group([host(2), host(1)]);
        let intent = group
            .intent(author(), EventBuilder::new(Kind::from(9u16)))
            .expect("a plain draft contextualizes");
        assert_eq!(routed(&intent), vec![host(1), host(2)]);
    }

    #[test]
    fn a_single_host_scope_still_routes_explicitly_to_that_one_host() {
        let intent = group([host(1)])
            .intent(author(), EventBuilder::new(Kind::from(9u16)))
            .expect("a plain draft contextualizes");
        assert_eq!(routed(&intent), vec![host(1)]);
    }

    /// An unsigned group write freezes an exact decoded author (#878); it
    /// never defers to whoever happens to be active at acceptance.
    #[test]
    fn an_unsigned_group_write_freezes_the_exact_author() {
        let me = author();
        let intent = group([host(1)])
            .intent(me, EventBuilder::new(Kind::from(9u16)))
            .expect("a plain draft contextualizes");
        assert_eq!(intent.identity, Identity::Explicit(me));
    }

    #[test]
    fn a_group_write_reaches_the_one_publish_door() {
        let engine = engine();
        let receipts = group([host(1), host(2)])
            .publish(
                &engine,
                author(),
                EventBuilder::new(Kind::from(9u16)).content("first light"),
            )
            .expect("the publish door accepts a group write");
        drop(receipts);
        engine.shutdown();
    }

    /// PROTOCOL-KINDBLINDNESS-004 (supported-facade half): an unfamiliar
    /// kind -- one NIP-29 does not define and no other well-known NIP names
    /// either -- is published, not questioned. The publish door accepts it
    /// exactly like an ordinary kind:9; nothing about the kind being
    /// unrecognised produces a refusal.
    #[test]
    fn an_unfamiliar_kind_is_published_not_questioned() {
        let engine = engine();
        let receipts = group([host(1)])
            .publish(
                &engine,
                author(),
                EventBuilder::new(Kind::from(44815u16)).content("whatever this is"),
            )
            .expect("an unfamiliar kind must be accepted by the publish door like any other");
        drop(receipts);
        engine.shutdown();
    }

    /// A caller error is decided BEFORE the door: no receipt stream is even
    /// returned, which is what "no write intent was accepted" means.
    #[test]
    fn a_caller_supplied_context_never_reaches_the_door() {
        let engine = engine();
        let refused = group([host(1)]).publish(
            &engine,
            author(),
            EventBuilder::new(Kind::from(9u16)).tag(Tag::parse(["h", GROUP]).unwrap()),
        );
        assert!(matches!(
            refused,
            Err(GroupPublishError::Context(
                GroupContextError::CallerSuppliedContext
            ))
        ));
        engine.shutdown();
    }

    /// PROTOCOL-PRESIGNEDPUBLICATION-001 (direct half): `signed_intent`
    /// mints its `WriteIntent` at this exact seam -- a `WritePayload::Signed`,
    /// never a `WritePayload::Event`. Proven here,
    /// with no relay and no engine I/O required to observe it, so the
    /// end-to-end wire proof in
    /// `crates/nmp/tests/group_publication_door.rs`'s
    /// `publish_signed_delivers_the_callers_exact_pre_signed_bytes_to_every_host`
    /// is checking the SAME mechanism this test pins, not a different one.
    #[test]
    fn a_pre_signed_event_is_carried_into_the_minted_intent_byte_for_byte() {
        let event = signed_event(&Keys::generate());
        let intent = group([host(1)])
            .signed_intent(event.clone())
            .expect("a correctly contextualized signed event mints");
        assert_eq!(routed(&intent), vec![host(1)]);
        match intent.payload {
            WritePayload::Signed(out) => assert_eq!(
                out, event,
                "the minted intent must carry the caller's signed event unchanged -- \
                 same id, same signature, same tags, same content"
            ),
            _ => {
                panic!("a pre-signed event must mint a Signed payload, not something else")
            }
        }
    }

    /// PROTOCOL-PRESIGNEDPUBLICATION-006 (direct half): the route is the
    /// group's own host set, minted from the scope the group was
    /// constructed with -- never derived from whichever key signed the
    /// event. Two different signers get the identical route and neither
    /// event is mutated.
    #[test]
    fn the_route_follows_the_group_not_whichever_key_signed_the_pre_signed_event() {
        let alice_signed = signed_event(&Keys::generate());
        let bob_signed = signed_event(&Keys::generate());
        assert_ne!(
            alice_signed.pubkey, bob_signed.pubkey,
            "the fixture must exercise two genuinely different signers"
        );
        let group = group([host(1), host(2)]);
        for event in [alice_signed, bob_signed] {
            let intent = group
                .signed_intent(event.clone())
                .expect("a correctly contextualized signed event mints");
            assert_eq!(
                routed(&intent),
                vec![host(1), host(2)],
                "the route is the group's hosts regardless of who signed the event"
            );
            match intent.payload {
                WritePayload::Signed(out) => assert_eq!(out, event),
                _ => panic!("a signed event must mint a Signed payload, not something else"),
            }
        }
    }

    /// #1242, the whole point: the door PRODUCES the intent. Everything the
    /// app would otherwise have had to choose -- the `h` row, the route, the
    /// author -- is already decided in the returned value, and nothing was
    /// published to learn it.
    #[test]
    fn the_mint_door_hands_back_a_fully_decided_intent_and_publishes_nothing() {
        let me = author();
        let intent = group([host(1), host(2)])
            .intent(me, EventBuilder::new(Kind::from(9u16)).content("first light"))
            .expect("a plain draft contextualizes");

        assert_eq!(
            routed(&intent),
            vec![host(1), host(2)],
            "the route is the group's whole scope, chosen by the door and not by the app"
        );
        assert_eq!(intent.identity, Identity::Explicit(me));
        assert_eq!(
            intent.correlation, None,
            "a correlation token is the caller's to mint and persist; the door must not invent one"
        );
        match &intent.payload {
            WritePayload::Event(builder) => {
                let context: Vec<&Tag> = builder
                    .tags
                    .iter()
                    .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("h"))
                    .collect();
                assert_eq!(context.len(), 1, "exactly one context row, appended by the door");
                assert_eq!(context[0].as_slice()[1], GROUP);
            }
            _ => panic!("an unsigned draft must mint an Event payload"),
        }
    }

    /// The refusals fire at MINT time -- earlier than they used to, which is
    /// where a caller error belongs. No engine is involved at all, so this
    /// cannot be passing because a door refused it downstream.
    #[test]
    fn a_caller_supplied_context_is_refused_before_any_intent_exists() {
        assert_eq!(
            group([host(1)])
                .intent(
                    author(),
                    EventBuilder::new(Kind::from(9u16)).tag(Tag::parse(["h", GROUP]).unwrap()),
                )
                .err(),
            Some(GroupPublishError::Context(
                GroupContextError::CallerSuppliedContext
            ))
        );
    }

    /// The SIGNED mint door carries the same context refusals the unsigned
    /// one does, over the whole set: no `h`, the wrong `h`, and -- the
    /// asymmetry a real consumer had left open in its own hand-rolled check
    /// -- more than one `h`. There is no spelling of an ambiguously-scoped
    /// group write on either door.
    #[test]
    fn the_signed_mint_door_refuses_every_ill_scoped_event_including_a_second_h_row() {
        let signer = Keys::generate();
        let signed = |tags: Vec<Tag>| {
            UnsignedEvent::new(
                signer.public_key(),
                Timestamp::from(1_700_000_000u64),
                Kind::from(9u16),
                tags,
                "first light".to_string(),
            )
            .sign_with_keys(&signer)
            .expect("fixture keys sign cleanly")
        };
        let group = group([host(1)]);

        assert_eq!(
            group.signed_intent(signed(Vec::new())).err(),
            Some(GroupPublishError::Context(
                GroupContextError::MissingContext {
                    expected: GROUP.to_string()
                }
            ))
        );
        assert_eq!(
            group
                .signed_intent(signed(vec![Tag::parse(["h", "darkroom"]).unwrap()]))
                .err(),
            Some(GroupPublishError::Context(
                GroupContextError::MismatchedContext {
                    found: "darkroom".to_string(),
                    expected: GROUP.to_string()
                }
            ))
        );
        assert_eq!(
            group
                .signed_intent(signed(vec![
                    Tag::parse(["h", GROUP]).unwrap(),
                    Tag::parse(["h", "darkroom"]).unwrap(),
                ]))
                .err(),
            Some(GroupPublishError::Context(
                GroupContextError::AmbiguousContext {
                    expected: GROUP.to_string()
                }
            )),
            "an event claiming two groups has no single answer and must never mint an intent"
        );
    }

    /// #1244: a group write minted through the door, stamped with the app's
    /// OWN crash-safe token and handed to the one publish door, is recovered
    /// by that token afterwards -- the exact receipt, by the exact id the app
    /// never had to see. This is the recovery path a group write did not have.
    #[test]
    fn a_group_write_is_reattachable_by_the_apps_own_correlation_token() {
        let engine = engine();
        let token = "group-write-0001";
        let mut intent = group([host(1)])
            .intent(author(), EventBuilder::new(Kind::from(9u16)).content("first light"))
            .expect("a plain draft contextualizes");
        intent.correlation = Some(
            nmp_grammar::CorrelationToken::try_from(token).expect("a short token is well-formed"),
        );

        let receipt = engine
            .publish_tracked(intent)
            .expect("the one publish door accepts a group-minted intent");
        let recovered = engine
            .reattach_by_correlation(token.to_string())
            .expect("the token lookup door answers");
        match recovered {
            crate::ReceiptReattachment::Attached { id, .. } => assert_eq!(
                id, receipt.id,
                "the token must resolve to the very receipt the group write was accepted as"
            ),
            _ => panic!("a correlated group write must be reattachable by its own token"),
        }
        engine.shutdown();
    }

    /// #1244's other half: even the INLINE door hands back the store-issued
    /// receipt id, because a group write is a tracked write. It was always
    /// allocated; it was simply dropped on the way out.
    #[test]
    fn the_inline_door_hands_back_the_store_issued_receipt_id() {
        let engine = engine();
        let receipt = group([host(1)])
            .publish(
                &engine,
                author(),
                EventBuilder::new(Kind::from(9u16)).content("first light"),
            )
            .expect("the publish door accepts a group write");
        let queued = engine.publish_queue().expect("the queue reads back");
        assert!(
            queued.iter().any(|entry| entry.receipt_id == receipt.id),
            "the id a group publication returns must name a real queue entry"
        );
        engine.shutdown();
    }

    /// Every named operation is an ordinary group publication: same door,
    /// same `h`, same whole-scope route. Exercised over the whole set rather
    /// than one representative, so a new operation cannot quietly acquire its
    /// own path.
    #[test]
    fn every_named_operation_takes_the_same_path() {
        let engine = engine();
        let group = group([host(1), host(2)]);
        let me = author();
        let subject = author();
        let calls: Vec<(&str, Result<ReceiptStream, GroupPublishError>)> = vec![
            (
                "join_request",
                group.join_request(&engine, me, Some("code")),
            ),
            ("leave_request", group.leave_request(&engine, me)),
            ("add_user", group.add_user(&engine, me, subject, None)),
            ("remove_user", group.remove_user(&engine, me, subject)),
            (
                "edit_metadata",
                group.edit_metadata(&engine, me, Some("Photographers"), None),
            ),
            (
                "delete_event",
                group.delete_event(&engine, me, EventId::from_slice(&[9; 32]).unwrap()),
            ),
            ("create_group", group.create_group(&engine, me)),
            ("delete_group", group.delete_group(&engine, me)),
            ("create_invite", group.create_invite(&engine, me, "code")),
        ];
        for (name, outcome) in calls {
            assert!(
                outcome.is_ok(),
                "{name} must reach the one publish door like every other group write"
            );
        }
        engine.shutdown();
    }

    /// The read half has no verb of its own: the group mints a live query and
    /// the app takes it through `Engine::observe`.
    #[test]
    fn the_read_half_is_a_live_query_the_ordinary_observe_door_takes() {
        let engine = engine();
        let query = group([host(1)])
            .read(Filter {
                kinds: Some(BTreeSet::from([9u16])),
                ..Filter::default()
            })
            .expect("a single-host group read is one branch");
        let subscription = engine
            .observe(query, None)
            .expect("a group read is an ordinary live query");
        drop(subscription);
        engine.shutdown();
    }

    /// PROTOCOL-READSTHROUGHTHEONEDOOR-004: one retained `Group` value mints
    /// several independent ordinary observations, live at once. There is no
    /// per-group subscription-count limit and no group-owned lifecycle that a
    /// second `.read(...)` would have to reconstruct or replace -- each call
    /// mints an ordinary `LiveQuery` and each `engine.observe(...)` call opens
    /// its own ordinary subscription, exactly like four unrelated reads would.
    #[test]
    fn one_group_value_mints_several_independent_simultaneous_observations() {
        let engine = engine();
        let group = group([host(1)]);

        let chat = group
            .read(Filter {
                kinds: Some(BTreeSet::from([9u16, 9000u16, 9001u16])),
                ..Filter::default()
            })
            .expect("a chat selection scopes");
        let activity = group
            .read(Filter {
                kinds: Some(BTreeSet::from([30315u16])),
                ..Filter::default()
            })
            .expect("an activity selection scopes");
        let reactions = group
            .read(Filter {
                kinds: Some(BTreeSet::from([7u16])),
                ..Filter::default()
            })
            .expect("a reactions selection scopes");

        let subscriptions = vec![
            engine
                .observe(chat, None)
                .expect("the chat query opens its own subscription"),
            engine
                .observe(activity, None)
                .expect("the activity query opens its own subscription"),
            engine
                .observe(reactions, None)
                .expect("the reactions query opens its own subscription"),
        ];
        // The roster is the fourth simultaneous observation, and it is a
        // records observation rather than a fourth `read`: the relay-signed
        // records are `d`-keyed and unreachable through the `#h` door (#1245).
        let roster = group
            .observe(&engine, [GroupRecord::Admins, GroupRecord::Members])
            .expect("the roster observation opens its own subscription");
        assert_eq!(
            subscriptions.len(),
            3,
            "all four independent observations must be open at once, from the SAME group value"
        );
        drop(subscriptions);
        drop(roster);
        engine.shutdown();
    }

    /// #1245, at the shipped call site that proved it. The variable was
    /// literally named `membership`, the read returned `Ok`, the subscription
    /// opened, and no 39001 or 39002 event could ever match the `#h` filter it
    /// built -- an empty result indistinguishable from a group with no roster
    /// published.
    #[test]
    fn a_roster_read_through_the_content_door_is_refused_not_silently_empty() {
        let group = group([host(1)]);
        assert_eq!(
            group
                .read(Filter {
                    kinds: Some(BTreeSet::from([39001u16, 39002u16])),
                    ..Filter::default()
                })
                .err(),
            Some(GroupReadError::Context(
                GroupContextError::RecordsAreNotContextScoped {
                    kinds: BTreeSet::from([39001u16, 39002u16])
                }
            )),
            "the door must say no; a door that returns nothing forever is worse"
        );
    }

    /// The refusal is about the `d`/`h` axis, not about kinds: ordinary group
    /// content -- including the moderation kinds NIP-29 itself defines, which
    /// DO carry `h` -- still reads through the same door untouched.
    #[test]
    fn ordinary_group_content_still_reads_through_the_content_door() {
        let group = group([host(1)]);
        for kinds in [
            BTreeSet::from([9u16]),
            BTreeSet::from([9000u16, 9001u16]),
            BTreeSet::from([30315u16]),
            BTreeSet::from([31337u16]),
        ] {
            assert!(
                group
                    .read(Filter {
                        kinds: Some(kinds.clone()),
                        ..Filter::default()
                    })
                    .is_ok(),
                "{kinds:?} lives IN a group and must still read through the content door"
            );
        }
    }
}
