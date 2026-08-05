//! [`Groups`] -- the several groups one write belongs to (#1281).
//!
//! [`Group`](super::Group) is a room: an identity an app reads, watches,
//! moderates and writes into. `Groups` is none of those. It is the WRITE
//! CONTEXT alone -- the hosts a scope named plus the set of group ids one
//! event claims -- and it exists because a legitimate NIP-29 write had no
//! door at all.
//!
//! It has exactly two methods and both are the UNSIGNED door: NMP appends the
//! `h` rows, NMP signs, and the app reads its own write back through the
//! subscription it already holds. There is deliberately no pre-signed
//! spelling here and no way to obtain a draft to sign yourself.
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
//! the contextualizer, correctly: the `h` row belongs to the door.
//!
//! So an app that needed this hand-minted a [`WriteIntent`] -- spelling its
//! own [`WriteRouting::Explicit`] and writing its own `h` rows -- which is
//! exactly what #1242 removed for every other group write.
//!
//! # No pre-signed door, and the evidence for why none is needed
//!
//! The consumer that reported this reached the multi-`h` shape by routing its
//! status through a PRE-SIGNED path, so it would be easy to assume a
//! pre-signed multi-group door is what it needs. Its own source says
//! otherwise. The unsigned path already computed the event id WITHOUT signing
//! -- an id is a hash of `(author, created_at, kind, tags, content)` and a
//! signature was never an input -- and the refusal that pushed status onto
//! the signed path said, verbatim, that "exact multi-group events must be
//! pre-signed". That was an ARITY limit, not an id-timing requirement, and
//! this type removes it. Nothing consumed the status id either: a kind:30315
//! status is addressable at `(author, d)`, and its reader is keyed by that
//! coordinate.
//!
//! So the several-group case needs the unsigned door and only the unsigned
//! door.
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
use nostr::{PublicKey, RelayUrl};

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

    /// Mint the contextualized [`WriteIntent`] for an unsigned draft, as
    /// `author`, and publish NOTHING.
    ///
    /// [`Group::intent`](super::Group::intent) with a larger set, and the
    /// same door: one `h` row per retained id appended BEFORE the stamp/sign
    /// step so every row is inside the bytes that get signed,
    /// [`WriteRouting::Explicit`] over the scope's whole host set, an exact
    /// frozen author (#878), and `correlation: None` for the caller to stamp
    /// (#1244).
    ///
    /// The app names neither a relay nor an `h` row. A draft that already
    /// carries an `h` is refused whichever value it holds, because the row
    /// belongs to the retained scope and not to the caller.
    pub fn intent(
        &self,
        author: PublicKey,
        builder: EventBuilder,
    ) -> Result<WriteIntent, GroupPublishError> {
        let contextualized = nmp_nip29::contextualize(&self.ids, builder)?;
        Ok(self.mint(
            WritePayload::Event(contextualized),
            Identity::Explicit(author),
        ))
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

    /// The one shape a group write has, and the ONLY place a group
    /// [`WriteIntent`] is assembled -- [`Group`](super::Group)'s doors mint
    /// through this one too, which is what makes "one group is the
    /// one-element case" a property of the code. `Explicit(every host)` comes
    /// from the scope the app named once; the `h` rows carry GROUP IDS, never
    /// relays, so the hosts are not derivable from the event and no resolver
    /// could compute them.
    pub(super) fn mint(&self, payload: WritePayload, identity: Identity) -> WriteIntent {
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
    use nostr::{Keys, Kind, Tag};

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

    /// The inline spelling reaches the same one publish door.
    #[test]
    fn the_inline_door_reaches_the_one_publish_door() {
        let engine = engine();
        let receipts = rooms()
            .publish(
                &engine,
                author(),
                nmp_grammar::EventBuilder::new(Kind::from(30315u16)),
            )
            .expect("the publish door accepts a multi-group write");
        drop(receipts);
        engine.shutdown();
    }
}
