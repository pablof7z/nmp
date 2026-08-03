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
//! Writes mint the ordinary opaque [`WriteIntent`] and hand it to the ONE
//! publish door; there is no group-shaped receipt and no group-shaped retry.

use std::collections::BTreeSet;

use nmp_grammar::{
    Durability, EventBuilder, Filter, Identity, WriteIntent, WritePayload, WriteRouting,
};
use nostr::{Event, EventId, PublicKey, RelayUrl};

use super::read::{self, GroupReadError};
use super::records::{GroupObservation, GroupObserveError};
use crate::delivery::WriteStatus;
use nmp_nip29::GroupRecord;
use crate::engine::Engine;
use crate::error::EngineError;
use crate::runtime::FifoReceiver;
use crate::LiveQuery;
use nmp_nip29::GroupContextError;

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

/// The receipt stream a group publication returns -- the SAME stream every
/// other publish returns, drained the same way.
pub type GroupReceipts = FifoReceiver<WriteStatus>;

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
    /// [`RelayScope::listing_branches`](super::RelayScope::listing_branches):
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
        let predicate = super::any_of([self.id.clone()]);
        let branches = self
            .hosts
            .iter()
            .map(|host| nmp_nip29::group_records_at(host, &records, predicate.lower_at(host)))
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

    /// Publish any unsigned draft into the group, as `author`.
    ///
    /// The group appends exactly one `["h", group_id]` row BEFORE the
    /// stamp/sign step, so the context tag is inside the bytes that get signed,
    /// and routes explicitly to every host in the scope.
    ///
    /// `author` is an exact decoded [`PublicKey`], never a reactive selector:
    /// a semantic group write freezes who is writing at composition time
    /// rather than resolving it later against whoever happens to be active
    /// (#878). Reactive identity remains entirely valid on the READ side.
    ///
    /// Kind-blind: no kind is privileged, refused, or read.
    pub fn publish(
        &self,
        engine: &Engine,
        author: PublicKey,
        builder: EventBuilder,
    ) -> Result<GroupReceipts, GroupPublishError> {
        let contextualized = nmp_nip29::contextualize(&self.id, builder)?;
        self.through_the_one_door(
            engine,
            self.intent(
                WritePayload::Event(contextualized),
                Identity::Explicit(author),
            ),
        )
    }

    /// Publish an ALREADY-SIGNED event into the group.
    ///
    /// The `h` it already carries is VALIDATED, never appended: appending
    /// would change the bytes and therefore the `EventId` the caller already
    /// has. A missing, wrong or duplicated `h` is a typed refusal, not a
    /// repair and not a re-sign.
    ///
    /// There is no author argument. A signed event already fixed its author;
    /// accepting a second selector would let the two disagree.
    pub fn publish_signed(
        &self,
        engine: &Engine,
        event: Event,
    ) -> Result<GroupReceipts, GroupPublishError> {
        nmp_nip29::validate_context(&self.id, &event)?;
        let author = event.pubkey;
        self.through_the_one_door(
            engine,
            self.intent(WritePayload::Signed(event), Identity::Explicit(author)),
        )
    }

    /// kind:9021 -- ask to join. Publishable with no subscription at all:
    /// writing into a group you cannot read yet is the case this door exists
    /// to support.
    pub fn join_request(
        &self,
        engine: &Engine,
        author: PublicKey,
        invite_code: Option<&str>,
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::join_request(invite_code))
    }

    /// kind:9022 -- leave.
    pub fn leave_request(
        &self,
        engine: &Engine,
        author: PublicKey,
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::leave_request())
    }

    /// kind:9000 -- add a member, optionally with a role.
    pub fn add_user(
        &self,
        engine: &Engine,
        author: PublicKey,
        pubkey: PublicKey,
        role: Option<&str>,
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::add_user(pubkey, role))
    }

    /// kind:9001 -- remove a member.
    pub fn remove_user(
        &self,
        engine: &Engine,
        author: PublicKey,
        pubkey: PublicKey,
    ) -> Result<GroupReceipts, GroupPublishError> {
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
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::edit_metadata(name, about))
    }

    /// kind:9005 -- delete one group-hosted event.
    pub fn delete_event(
        &self,
        engine: &Engine,
        author: PublicKey,
        event_id: EventId,
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::delete_event(event_id))
    }

    /// kind:9007 -- create the group at its hosts.
    pub fn create_group(
        &self,
        engine: &Engine,
        author: PublicKey,
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::create_group())
    }

    /// kind:9008 -- delete the group from its hosts.
    pub fn delete_group(
        &self,
        engine: &Engine,
        author: PublicKey,
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::delete_group())
    }

    /// kind:9009 -- mint an invite code redeemable by
    /// [`join_request`](Self::join_request).
    pub fn create_invite(
        &self,
        engine: &Engine,
        author: PublicKey,
        code: &str,
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, author, nmp_nip29::create_invite(code))
    }

    /// The one shape a group write has. `Explicit(every host)` is minted
    /// HERE, from the scope the app named once: no caller supplies it and no
    /// engine derives it. The `h` tag carries the GROUP ID, never a relay, so
    /// the hosts are not derivable from the event and no resolver could ever
    /// compute them.
    fn intent(&self, payload: WritePayload, identity: Identity) -> WriteIntent {
        WriteIntent {
            payload,
            durability: Durability::Durable,
            routing: WriteRouting::Explicit(self.hosts.iter().cloned().collect()),
            identity,
            correlation: None,
        }
    }

    /// The whole engine-facing body of this type: hand a group-minted intent
    /// to the one publish door. Named so a reader can see there is exactly
    /// one, and so a second write lifecycle could not be added without
    /// deleting this line.
    fn through_the_one_door(
        &self,
        engine: &Engine,
        intent: WriteIntent,
    ) -> Result<GroupReceipts, GroupPublishError> {
        engine.publish(intent).map_err(GroupPublishError::Engine)
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
        let intent = group.intent(
            WritePayload::Event(EventBuilder::new(Kind::from(9u16))),
            Identity::Explicit(author()),
        );
        assert_eq!(routed(&intent), vec![host(1), host(2)]);
    }

    #[test]
    fn a_single_host_scope_still_routes_explicitly_to_that_one_host() {
        let intent = group([host(1)]).intent(
            WritePayload::Event(EventBuilder::new(Kind::from(9u16))),
            Identity::Explicit(author()),
        );
        assert_eq!(routed(&intent), vec![host(1)]);
    }

    /// An unsigned group write freezes an exact decoded author (#878); it
    /// never defers to whoever happens to be active at acceptance.
    #[test]
    fn an_unsigned_group_write_freezes_the_exact_author() {
        let me = author();
        let intent = group([host(1)]).intent(
            WritePayload::Event(EventBuilder::new(Kind::from(9u16))),
            Identity::Explicit(me),
        );
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

    /// PROTOCOL-PRESIGNEDPUBLICATION-001 (direct half): `publish_signed`
    /// mints its `WriteIntent` at this exact seam -- `Self::intent` with a
    /// `WritePayload::Signed`, never a `WritePayload::Event`. Proven here,
    /// with no relay and no engine I/O required to observe it, so the
    /// end-to-end wire proof in
    /// `crates/nmp/tests/group_publication_door.rs`'s
    /// `publish_signed_delivers_the_callers_exact_pre_signed_bytes_to_every_host`
    /// is checking the SAME mechanism this test pins, not a different one.
    #[test]
    fn a_pre_signed_event_is_carried_into_the_minted_intent_byte_for_byte() {
        let event = signed_event(&Keys::generate());
        let intent = group([host(1)]).intent(
            WritePayload::Signed(event.clone()),
            Identity::Explicit(event.pubkey),
        );
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
            let intent = group.intent(
                WritePayload::Signed(event.clone()),
                Identity::Explicit(event.pubkey),
            );
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
        let calls: Vec<(&str, Result<GroupReceipts, GroupPublishError>)> = vec![
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
