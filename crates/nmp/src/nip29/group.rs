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
//! Reads mint a [`LiveQuery`] the ordinary observe door takes. There is
//! deliberately no `Group::observe`: a second read door onto the same
//! mechanism is exactly the shape #838 deleted on the write side. Writes mint
//! the ordinary opaque [`WriteIntent`] and hand it to the ONE publish door;
//! there is no group-shaped receipt and no group-shaped retry.

use std::collections::BTreeSet;

use nmp_grammar::{
    Durability, EventBuilder, Filter, Identity, WriteIntent, WritePayload, WriteRouting,
};
use nostr::{Event, EventId, PublicKey, RelayUrl};

use super::read::{self, GroupReadError};
use crate::delivery::WriteStatus;
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
        let mut branches = Vec::with_capacity(self.hosts.len());
        for host in &self.hosts {
            branches.push(nmp_nip29::group_demand_at(host, &self.id, selection.clone())?);
        }
        read::one_live_query(branches)
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
            self.intent(WritePayload::Event(contextualized), Identity::Explicit(author)),
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
    use nostr::{Keys, Kind, Tag};

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
}
