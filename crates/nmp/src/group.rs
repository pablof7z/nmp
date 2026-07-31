//! The engine binding for [`nip29::Group`](nmp_nip29::Group) (#977).
//!
//! `nmp-nip29` composes; this module publishes. The split is the whole point
//! of the design: `Group` and every NIP-29 composer are pure values over
//! `nostr` + `nmp-grammar` and know nothing about an engine, a route or a
//! signer, so `crates/nmp-nip29/Cargo.toml` stays free of a core or mechanism
//! edge (`scripts/check-nip29-ownership.sh`). The `&engine` ergonomics the
//! design asks for -- `group.publish(&engine, builder)` -- are recovered here
//! as an EXTENSION TRAIT implemented for `Group`, re-exported from the facade
//! so it is in scope by default. The dependency therefore runs
//! `nmp -> nmp-nip29`, never the other way.
//!
//! Every method here is thin by construction: it composes through
//! `nmp-nip29`, then hands the minted [`WriteIntent`] to the ONE publish door
//! ([`Engine::publish`]). There is no second write lifecycle, no group-shaped
//! receipt, no group-shaped retry -- a group publication is an ordinary
//! publication whose `h` row and whose `Explicit([host])` route were minted by
//! the group rather than spelled by the app (standing rule 4; #838 deleted the
//! second write lifecycle that was the opposite of this).
//!
//! There is deliberately no read verb here. Reads go through the one read
//! door: `engine.observe(LiveQuery(group.demand(filter)), None)`.

use nmp_grammar::{EventBuilder, WriteIntent};
use nmp_nip29::{Group, GroupContextError};
use nostr::{Event, EventId, PublicKey};

use crate::delivery::WriteStatus;
use crate::engine::Engine;
use crate::error::EngineError;
use crate::runtime::FifoReceiver;

/// Why a group publication never reached the publish door, or what the door
/// said when it did.
///
/// The two halves are kept apart because they are different kinds of fact: a
/// [`Self::Context`] is a CALLER error decided before anything was accepted --
/// no signature, no journal row, no receipt -- while a [`Self::Engine`] is the
/// ordinary publish door refusing the intent. Neither is a relay rejection;
/// a host that refuses the event does so on the receipt stream, like every
/// other write.
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

/// Publishing into a NIP-29 group, and NIP-29's own named operations.
///
/// Implemented for [`nip29::Group`](nmp_nip29::Group) and re-exported from the
/// facade, so `group.publish(&engine, builder)` compiles with no import of
/// its own.
///
/// NOTHING HERE ACCEPTS A RELAY, A ROUTING VALUE, OR AN `h` VALUE. The group
/// carries its host from construction and mints both the route and the
/// context row itself; that is the boundary, stated as a signature rather
/// than as a convention.
pub trait GroupOperations {
    /// Publish any unsigned draft into the group. The group appends exactly
    /// one `["h", group_id]` row BEFORE the stamp/sign step, so the context
    /// tag is inside the bytes that get signed, and routes explicitly to its
    /// own host.
    ///
    /// Kind-blind: no kind is privileged, refused, or read.
    fn publish(
        &self,
        engine: &Engine,
        builder: EventBuilder,
    ) -> Result<GroupReceipts, GroupPublishError>;

    /// Publish an ALREADY-SIGNED event into the group. The `h` it already
    /// carries is VALIDATED, never appended: appending would change the bytes
    /// and therefore the `EventId` the caller already has. A missing, wrong
    /// or duplicated `h` is a typed refusal, not a repair and not a re-sign.
    fn publish_signed(
        &self,
        engine: &Engine,
        event: Event,
    ) -> Result<GroupReceipts, GroupPublishError>;

    /// kind:9021 -- ask to join. Publishable with no subscription at all:
    /// writing into a group you cannot read yet is the case this door exists
    /// to support.
    fn join_request(
        &self,
        engine: &Engine,
        invite_code: Option<&str>,
    ) -> Result<GroupReceipts, GroupPublishError>;

    /// kind:9022 -- leave.
    fn leave_request(&self, engine: &Engine) -> Result<GroupReceipts, GroupPublishError>;

    /// kind:9000 -- add a member, optionally with a role.
    fn add_user(
        &self,
        engine: &Engine,
        pubkey: PublicKey,
        role: Option<&str>,
    ) -> Result<GroupReceipts, GroupPublishError>;

    /// kind:9001 -- remove a member.
    fn remove_user(
        &self,
        engine: &Engine,
        pubkey: PublicKey,
    ) -> Result<GroupReceipts, GroupPublishError>;

    /// kind:9002 -- set the group's display fields. An omitted field emits no
    /// tag at all, so it is left untouched rather than cleared.
    fn edit_metadata(
        &self,
        engine: &Engine,
        name: Option<&str>,
        about: Option<&str>,
    ) -> Result<GroupReceipts, GroupPublishError>;

    /// kind:9005 -- delete one group-hosted event.
    fn delete_event(
        &self,
        engine: &Engine,
        event_id: EventId,
    ) -> Result<GroupReceipts, GroupPublishError>;

    /// kind:9007 -- create the group at its host.
    fn create_group(&self, engine: &Engine) -> Result<GroupReceipts, GroupPublishError>;

    /// kind:9008 -- delete the group from its host.
    fn delete_group(&self, engine: &Engine) -> Result<GroupReceipts, GroupPublishError>;

    /// kind:9009 -- mint an invite code redeemable by
    /// [`join_request`](Self::join_request).
    fn create_invite(
        &self,
        engine: &Engine,
        code: &str,
    ) -> Result<GroupReceipts, GroupPublishError>;
}

impl GroupOperations for Group {
    fn publish(
        &self,
        engine: &Engine,
        builder: EventBuilder,
    ) -> Result<GroupReceipts, GroupPublishError> {
        through_the_one_door(engine, self.write_intent(builder)?)
    }

    fn publish_signed(
        &self,
        engine: &Engine,
        event: Event,
    ) -> Result<GroupReceipts, GroupPublishError> {
        through_the_one_door(engine, self.signed_write_intent(event)?)
    }

    fn join_request(
        &self,
        engine: &Engine,
        invite_code: Option<&str>,
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, nmp_nip29::join_request(invite_code))
    }

    fn leave_request(&self, engine: &Engine) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, nmp_nip29::leave_request())
    }

    fn add_user(
        &self,
        engine: &Engine,
        pubkey: PublicKey,
        role: Option<&str>,
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, nmp_nip29::add_user(pubkey, role))
    }

    fn remove_user(
        &self,
        engine: &Engine,
        pubkey: PublicKey,
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, nmp_nip29::remove_user(pubkey))
    }

    fn edit_metadata(
        &self,
        engine: &Engine,
        name: Option<&str>,
        about: Option<&str>,
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, nmp_nip29::edit_metadata(name, about))
    }

    fn delete_event(
        &self,
        engine: &Engine,
        event_id: EventId,
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, nmp_nip29::delete_event(event_id))
    }

    fn create_group(&self, engine: &Engine) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, nmp_nip29::create_group())
    }

    fn delete_group(&self, engine: &Engine) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, nmp_nip29::delete_group())
    }

    fn create_invite(
        &self,
        engine: &Engine,
        code: &str,
    ) -> Result<GroupReceipts, GroupPublishError> {
        self.publish(engine, nmp_nip29::create_invite(code))
    }
}

/// The whole engine-facing body of this module: hand a group-minted intent to
/// the one publish door. Named so a reader can see there is exactly one, and
/// so a second write lifecycle could not be added without deleting this line.
fn through_the_one_door(
    engine: &Engine,
    intent: WriteIntent,
) -> Result<GroupReceipts, GroupPublishError> {
    engine.publish(intent).map_err(GroupPublishError::Engine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::{Filter, WritePayload, WriteRouting};
    use nostr::{Kind, RelayUrl, Tag};

    fn engine() -> Engine {
        Engine::new(crate::config::EngineConfig::default()).expect("an in-memory engine builds")
    }

    fn group() -> Group {
        Group::new(
            RelayUrl::parse("wss://groups.example.com").expect("a well-formed host"),
            "photographers",
        )
    }

    /// The whole contract of this module in one assertion: a group publication
    /// reaches the ONE publish door, and reaches it carrying the `h` row and
    /// the explicit route the group minted. Read off the intent the group
    /// hands over, because that is the value this module forwards unchanged.
    #[test]
    fn a_group_write_reaches_the_one_publish_door_with_the_group_s_own_route() {
        let group = group();
        let intent = group
            .write_intent(EventBuilder::new(Kind::from(9u16)).content("first light"))
            .expect("a plain draft is contextualizable");
        match (&intent.payload, &intent.routing) {
            (WritePayload::Event(builder), WriteRouting::Explicit(relays)) => {
                assert_eq!(relays, &vec![group.host().clone()]);
                assert_eq!(
                    builder
                        .tags
                        .last()
                        .expect("the h row is appended")
                        .as_slice(),
                    &["h".to_string(), "photographers".to_string()]
                );
            }
            _ => panic!("a group draft is an Event payload on an Explicit route"),
        }

        let engine = engine();
        let receipts = group
            .publish(
                &engine,
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
        let refused = group().publish(
            &engine,
            EventBuilder::new(Kind::from(9u16)).tag(Tag::parse(["h", "photographers"]).unwrap()),
        );
        assert!(matches!(
            refused,
            Err(GroupPublishError::Context(
                GroupContextError::CallerSuppliedContext
            ))
        ));
        engine.shutdown();
    }

    /// Every named operation is an ordinary group publication: same door, same
    /// `h`, same route. Exercised over the whole set rather than one
    /// representative, so a new operation cannot quietly acquire its own path.
    #[test]
    fn every_named_operation_takes_the_same_path() {
        let engine = engine();
        let group = group();
        let subject = nostr::Keys::generate().public_key();
        let calls: Vec<(&str, Result<GroupReceipts, GroupPublishError>)> = vec![
            ("join_request", group.join_request(&engine, Some("code"))),
            ("leave_request", group.leave_request(&engine)),
            ("add_user", group.add_user(&engine, subject, None)),
            ("remove_user", group.remove_user(&engine, subject)),
            (
                "edit_metadata",
                group.edit_metadata(&engine, Some("Photographers"), None),
            ),
            (
                "delete_event",
                group.delete_event(&engine, nostr::EventId::from_slice(&[9; 32]).unwrap()),
            ),
            ("create_group", group.create_group(&engine)),
            ("delete_group", group.delete_group(&engine)),
            ("create_invite", group.create_invite(&engine, "code")),
        ];
        for (name, outcome) in calls {
            assert!(
                outcome.is_ok(),
                "{name} must reach the one publish door like every other group write"
            );
        }
        engine.shutdown();
    }

    /// The read half has no verb of its own: the group mints a `Demand` and the
    /// app takes it through `Engine::observe`.
    #[test]
    fn the_read_half_is_a_demand_the_ordinary_observe_door_takes() {
        let engine = engine();
        let demand = group().demand(Filter {
            kinds: Some(std::collections::BTreeSet::from([9u16])),
            ..Filter::default()
        });
        let subscription = engine
            .observe(nmp_resolver::LiveQuery(demand), None)
            .expect("a group demand is an ordinary live query");
        drop(subscription);
        engine.shutdown();
    }
}
