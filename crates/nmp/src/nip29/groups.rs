//! [`Groups`] -- the several groups one write belongs to (#1281).
//!
//! [`Group`](super::Group) is a room: an identity an app reads, watches,
//! moderates and writes into. `Groups` is none of those. It is the WRITE
//! CONTEXT alone -- the hosts a scope named plus the set of group ids one
//! event claims -- and it exists because a legitimate NIP-29 write had no
//! door at all.
//!
//! # The write that had no door
//!
//! A kind:30315 session status is addressable at `(author, d=status)` and
//! carries one `h` row per room the session currently occupies, so the same
//! status renders in every room the author is in. One event, one replaceable
//! coordinate, several groups.
//!
//! None of the obvious spellings works. Publishing it once per room makes
//! every copy REPLACE the last, because they share the coordinate -- the
//! author ends up visible in exactly one room, whichever wrote last. Picking
//! one room and dropping the rest changes what the event says. Composing it
//! under one room and letting the app append the other rows is refused by
//! [`Group::contextualize`](super::Group::contextualize), correctly: the `h`
//! row belongs to the door.
//!
//! So an app that needed this hand-minted a [`WriteIntent`] -- spelling its
//! own [`WriteRouting::Explicit`] and writing its own `h` rows -- which is
//! exactly what #1242 removed for every other group write.
//!
//! # It is the same door, at a larger arity
//!
//! There is no second mechanism here. [`Group`](super::Group)'s whole write
//! half IS a one-element `Groups`: `Group::intent` builds one and calls
//! [`Groups::intent`], and `nmp_nip29::contextualize` takes a set at every
//! call site in the workspace. The one-group case is not a special path that
//! happens to agree with this one -- it is literally this one.
//!
//! # What it deliberately is NOT
//!
//! No read door, no records observation, no named operation. NIP-29's
//! relay-signed records key themselves by `d` PER GROUP, a roster is one
//! group's, and every 9000-9022 moderation action names one group by
//! definition -- so a plural of any of them would be an aggregate this crate
//! would have to invent a meaning for. A write is the one thing that is
//! genuinely plural, and it is the only thing this type does.
//!
//! Nonempty by construction, the same shape [`nip29::on`](super::on) has for
//! relays: [`RelayScope::groups`](super::RelayScope::groups) is the only way
//! to make one and it refuses an empty set, because an event with no `h` row
//! is not in a group at all.

use std::collections::BTreeSet;

use nmp_grammar::{EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};
use nostr::{Event, PublicKey, RelayUrl};

use super::group::GroupPublishError;
use crate::engine::Engine;
use crate::runtime::ReceiptStream;
use nmp_nip29::GroupContextError;

/// The groups one write belongs to, on the relays their scope named.
///
/// Retains both privately, exactly as [`Group`](super::Group) does: no host
/// accessor, no id accessor, and no method that takes a per-call host, route,
/// group id or raw `h` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Groups {
    hosts: BTreeSet<RelayUrl>,
    ids: BTreeSet<String>,
}

impl Groups {
    /// Form a write context over a nonempty set of group ids.
    ///
    /// Fallible for the one reason [`nip29::on`](super::on) is: the set is
    /// caller-supplied and a caller-supplied set can be empty. Refusing here
    /// is what makes every method below infallible with respect to the group
    /// set.
    pub(super) fn new(
        hosts: BTreeSet<RelayUrl>,
        ids: BTreeSet<String>,
    ) -> Result<Self, GroupContextError> {
        debug_assert!(!hosts.is_empty(), "a scope proves its host set is nonempty");
        if ids.is_empty() {
            return Err(GroupContextError::NoGroupNamed);
        }
        Ok(Self { hosts, ids })
    }

    /// Apply the retained group ids to a draft the CALLER will sign itself
    /// (#1283, at this arity).
    ///
    /// One `h` row per retained id, appended before anything is signed, and
    /// the ids are named nowhere by the caller. Hand the signed result back
    /// to [`Self::signed_intent`] and the write is complete without the ids
    /// ever being spelled at all:
    ///
    /// ```text
    /// let signed = sign(groups.contextualize(builder)?, keys);
    /// let intent = groups.signed_intent(signed)?;
    /// ```
    ///
    /// A draft that already carries an `h` row is refused, whichever value it
    /// holds -- the refusal is about who owns the row.
    pub fn contextualize(&self, builder: EventBuilder) -> Result<EventBuilder, GroupContextError> {
        nmp_nip29::contextualize(&self.ids, builder)
    }

    /// Ask whether an already-signed event names exactly these groups,
    /// without building a write out of it.
    pub fn validate_context(&self, event: &Event) -> Result<(), GroupContextError> {
        nmp_nip29::validate_context(&self.ids, event)
    }

    /// Mint the contextualized [`WriteIntent`] for an unsigned draft, as
    /// `author`, and publish NOTHING.
    ///
    /// [`Group::intent`](super::Group::intent) with a larger set: the same
    /// appended-before-signing rows, the same [`WriteRouting::Explicit`] over
    /// the scope's whole host set, the same frozen exact author (#878), and
    /// the same `correlation: None` for the caller to stamp (#1244).
    pub fn intent(
        &self,
        author: PublicKey,
        builder: EventBuilder,
    ) -> Result<WriteIntent, GroupPublishError> {
        let contextualized = self.contextualize(builder)?;
        Ok(self.mint(
            WritePayload::Event(contextualized),
            Identity::Explicit(author),
        ))
    }

    /// Mint the contextualized [`WriteIntent`] for an ALREADY-SIGNED event,
    /// and publish nothing.
    ///
    /// The `h` rows it already carries are VALIDATED against the retained
    /// set, never appended: appending would change the bytes and therefore
    /// the `EventId` the caller already has. A set that is too small, too
    /// large, wrong, absent, or right-but-repeated is a typed refusal.
    pub fn signed_intent(&self, event: Event) -> Result<WriteIntent, GroupPublishError> {
        self.validate_context(&event)?;
        let author = event.pubkey;
        Ok(self.mint(WritePayload::Signed(event), Identity::Explicit(author)))
    }

    /// [`Self::intent`] handed straight to the one publish door.
    pub fn publish(
        &self,
        engine: &Engine,
        author: PublicKey,
        builder: EventBuilder,
    ) -> Result<ReceiptStream, GroupPublishError> {
        engine
            .publish_tracked(self.intent(author, builder)?)
            .map_err(GroupPublishError::Engine)
    }

    /// [`Self::signed_intent`] handed straight to the one publish door.
    pub fn publish_signed(
        &self,
        engine: &Engine,
        event: Event,
    ) -> Result<ReceiptStream, GroupPublishError> {
        engine
            .publish_tracked(self.signed_intent(event)?)
            .map_err(GroupPublishError::Engine)
    }

    /// The one shape a group write has, identical to
    /// [`Group`](super::Group)'s: `Explicit(every host)` minted from the
    /// scope the app named once. The `h` rows carry GROUP IDS, never relays,
    /// so the hosts are not derivable from the event and no resolver could
    /// compute them.
    fn mint(&self, payload: WritePayload, identity: Identity) -> WriteIntent {
        WriteIntent {
            payload,
            routing: WriteRouting::Explicit(self.hosts.iter().cloned().collect()),
            identity,
            correlation: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nip29;
    use nostr::{Keys, Kind, Tag, Timestamp, UnsignedEvent};

    fn host(n: u16) -> RelayUrl {
        RelayUrl::parse(&format!("wss://host-{n}.example.com")).expect("a well-formed host")
    }

    fn engine() -> Engine {
        Engine::new(crate::config::EngineConfig::default()).expect("an in-memory engine builds")
    }

    fn author() -> PublicKey {
        Keys::generate().public_key()
    }

    fn rooms() -> Groups {
        nip29::on([host(1), host(2)])
            .expect("a nonempty scope")
            .groups(["darkroom", "photographers"])
            .expect("a nonempty group set")
    }

    fn routed(intent: &WriteIntent) -> Vec<RelayUrl> {
        match &intent.routing {
            WriteRouting::Explicit(relays) => relays.clone(),
            WriteRouting::Auto => panic!("a group write is never Auto"),
        }
    }

    fn context_rows(intent: &WriteIntent) -> Vec<String> {
        match &intent.payload {
            WritePayload::Event(builder) => builder
                .tags
                .iter()
                .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("h"))
                .map(|tag| tag.as_slice()[1].clone())
                .collect(),
            _ => panic!("an unsigned draft must mint an Event payload"),
        }
    }

    fn status(tags: Vec<Tag>, signer: &Keys) -> Event {
        UnsignedEvent::new(
            signer.public_key(),
            Timestamp::from(1_700_000_000u64),
            Kind::from(30315u16),
            tags,
            String::new(),
        )
        .sign_with_keys(signer)
        .expect("fixture keys sign cleanly")
    }

    /// THE #1281 falsifier: one minted intent, one addressable coordinate,
    /// and one `h` row per room -- from a door, with the app naming neither
    /// a relay nor an `h` row. Before this the app hand-built the
    /// `WriteIntent` because no door would mint it.
    #[test]
    fn one_intent_carries_every_group_and_the_apps_own_coordinate() {
        let intent = rooms()
            .intent(
                author(),
                nmp_grammar::EventBuilder::new(Kind::from(30315u16))
                    .tag(Tag::parse(["d", "status"]).unwrap()),
            )
            .expect("a plain draft contextualizes for several groups");
        assert_eq!(
            context_rows(&intent),
            vec!["darkroom".to_string(), "photographers".to_string()],
            "one h row per room, so the one replaceable event renders in both"
        );
        assert_eq!(
            routed(&intent),
            vec![host(1), host(2)],
            "the route is the scope's whole host set, chosen by the door"
        );
        assert_eq!(intent.correlation, None);
    }

    /// The whole reason a multi-`h` write is the only correct shape: two
    /// separate one-room writes share `(author, d)` and so replace each
    /// other. Proven by construction -- the two single-room intents carry
    /// the IDENTICAL addressable coordinate, so a relay applying NIP-01
    /// replacement keeps one of them.
    #[test]
    fn publishing_once_per_room_would_share_one_replaceable_coordinate() {
        let scope = nip29::on([host(1)]).expect("a nonempty scope");
        let me = author();
        let draft = || {
            nmp_grammar::EventBuilder::new(Kind::from(30315u16))
                .tag(Tag::parse(["d", "status"]).unwrap())
        };
        let first = scope
            .group("darkroom")
            .intent(me, draft())
            .expect("a plain draft contextualizes");
        let second = scope
            .group("photographers")
            .intent(me, draft())
            .expect("a plain draft contextualizes");
        let coordinate = |intent: &WriteIntent| match (&intent.payload, &intent.identity) {
            (WritePayload::Event(builder), Identity::Explicit(author)) => (
                builder.kind,
                *author,
                builder
                    .tags
                    .iter()
                    .find(|tag| tag.as_slice().first().map(String::as_str) == Some("d"))
                    .map(|tag| tag.as_slice()[1].clone()),
            ),
            _ => panic!("an unsigned draft must mint an Event payload"),
        };
        assert_eq!(
            coordinate(&first),
            coordinate(&second),
            "NOTHING TO OBSERVE unless the two per-room copies really do collide"
        );

        let together = scope
            .groups(["darkroom", "photographers"])
            .expect("a nonempty group set")
            .intent(me, draft())
            .expect("a plain draft contextualizes");
        assert_eq!(
            context_rows(&together),
            vec!["darkroom".to_string(), "photographers".to_string()],
            "the multi-h write is the only spelling that survives the collision"
        );
    }

    /// #1283 at this arity: an app that signs its own bytes names the rooms
    /// ONCE. The ids never appear in app code between composition and mint.
    #[test]
    fn a_self_signed_write_names_the_rooms_once_and_mints_from_the_same_value() {
        let signer = Keys::generate();
        let rooms = rooms();
        let draft = rooms
            .contextualize(
                nmp_grammar::EventBuilder::new(Kind::from(30315u16))
                    .tag(Tag::parse(["d", "status"]).unwrap()),
            )
            .expect("the retained ids contextualize the caller's own draft");
        let signed = UnsignedEvent::new(
            signer.public_key(),
            Timestamp::from(1_700_000_000u64),
            draft.kind,
            draft.tags.clone(),
            draft.content.clone(),
        )
        .sign_with_keys(&signer)
        .expect("fixture keys sign cleanly");
        let known_id = signed.id;

        let intent = rooms
            .signed_intent(signed)
            .expect("bytes this very value contextualized must validate against it");
        match intent.payload {
            WritePayload::Signed(out) => assert_eq!(
                out.id, known_id,
                "the id the app already showed on screen survives the mint"
            ),
            _ => panic!("a pre-signed event must mint a Signed payload"),
        }
    }

    /// The pre-signed refusals hold over the SET: a status missing one room
    /// is refused rather than published into the rooms it did name, because
    /// a silently narrowed status stops rendering where the app believes it
    /// still shows.
    #[test]
    fn a_signed_write_missing_one_room_is_refused_not_narrowed() {
        let signer = Keys::generate();
        let short = status(vec![Tag::parse(["h", "darkroom"]).unwrap()], &signer);
        assert_eq!(
            rooms().signed_intent(short).err(),
            Some(GroupPublishError::Context(
                GroupContextError::MismatchedContext {
                    found: BTreeSet::from(["darkroom".to_string()]),
                    expected: BTreeSet::from(["darkroom".to_string(), "photographers".to_string()]),
                }
            ))
        );
    }

    /// A write into no group is refused where the value is FORMED, so no
    /// `Groups` exists on that path at all -- the invalid state is
    /// unconstructible rather than validated later, exactly as an empty
    /// relay set is.
    #[test]
    fn a_write_context_over_no_group_is_never_formed() {
        let empty: [&str; 0] = [];
        assert_eq!(
            nip29::on([host(1)])
                .expect("a nonempty scope")
                .groups(empty)
                .err(),
            Some(GroupContextError::NoGroupNamed)
        );
    }

    /// Duplicates collapse and order is the id order, so two apps naming the
    /// same rooms differently hold the SAME value and compose the same
    /// bytes.
    #[test]
    fn duplicate_and_unsorted_ids_canonicalize_to_one_set() {
        let scope = nip29::on([host(1)]).expect("a nonempty scope");
        assert_eq!(
            scope.groups(["b", "a", "b"]).expect("two rooms"),
            scope.groups(["a", "b"]).expect("two rooms")
        );
    }

    /// A caller-supplied `h` is refused at this arity too, and it is refused
    /// BEFORE any intent exists -- where a caller error belongs. Named
    /// distinctly from `Group`'s own one-group proof so a governed
    /// `nmp:evidence` locator resolves to exactly one executable test.
    #[test]
    fn a_caller_supplied_context_is_refused_before_any_several_group_intent_exists() {
        assert_eq!(
            rooms()
                .intent(
                    author(),
                    nmp_grammar::EventBuilder::new(Kind::from(30315u16))
                        .tag(Tag::parse(["h", "darkroom"]).unwrap()),
                )
                .err(),
            Some(GroupPublishError::Context(
                GroupContextError::CallerSuppliedContext
            ))
        );
    }

    /// A multi-group write is an ordinary tracked write through the ONE
    /// publish door -- no second lifecycle, and reattachable by the app's own
    /// correlation token like every other.
    #[test]
    fn a_multi_group_write_is_an_ordinary_tracked_write() {
        let engine = engine();
        let token = "multi-group-write-0001";
        let mut intent = rooms()
            .intent(
                author(),
                nmp_grammar::EventBuilder::new(Kind::from(30315u16)),
            )
            .expect("a plain draft contextualizes");
        intent.correlation = Some(
            nmp_grammar::CorrelationToken::try_from(token).expect("a short token is well-formed"),
        );
        let receipt = engine
            .publish_tracked(intent)
            .expect("the one publish door accepts a multi-group intent");
        match engine
            .reattach_by_correlation(token.to_string())
            .expect("the token lookup door answers")
        {
            crate::ReceiptReattachment::Attached { id, .. } => assert_eq!(id, receipt.id),
            _ => panic!("a correlated multi-group write must be reattachable by its own token"),
        }
        engine.shutdown();
    }

    /// The inline spellings reach the same door.
    #[test]
    fn the_inline_doors_reach_the_one_publish_door() {
        let engine = engine();
        let rooms = rooms();
        let receipts = rooms
            .publish(
                &engine,
                author(),
                nmp_grammar::EventBuilder::new(Kind::from(30315u16)),
            )
            .expect("the publish door accepts a multi-group write");
        drop(receipts);

        let signer = Keys::generate();
        let complete = status(
            vec![
                Tag::parse(["h", "darkroom"]).unwrap(),
                Tag::parse(["h", "photographers"]).unwrap(),
            ],
            &signer,
        );
        let receipts = rooms
            .publish_signed(&engine, complete)
            .expect("the publish door accepts a pre-signed multi-group write");
        drop(receipts);
        engine.shutdown();
    }
}
