//! [`Groups`] -- the several groups one write belongs to (#1281).
//!
//! [`Group`](super::Group) is a room: an identity an app reads, watches,
//! moderates and writes into. `Groups` is none of those. It is the WRITE
//! CONTEXT alone -- the hosts a scope named plus the set of group ids one
//! event claims -- and it exists because a legitimate NIP-29 write had no
//! door at all.
//!
//! It has exactly ONE method, [`Groups::publish`]. NMP appends the `h` rows,
//! NMP signs, NMP publishes, and the app reads its own write back through the
//! subscription it already holds. There is deliberately no pre-signed
//! spelling, no way to obtain a draft to sign yourself, and no
//! mint-without-publish door -- see "One door, not three" below.
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
//! # One door, not three
//!
//! An earlier shape of this type also handed back a [`WriteIntent`] for an
//! app to submit later. That door is absent, and the reasons are checkable
//! rather than stylistic: a `WriteIntent` derives NOTHING -- no `Clone`, no
//! `Debug`, no `Serialize` -- so it cannot be persisted across a restart,
//! cloned for batching, or inspected, and holding one buys an app nothing.
//! Its one honest use is stamping [`WriteIntent::correlation`], and the
//! consumer that asked for the door stamps `None` on every intent it builds.
//! The separate submit stage that motivated it had already been deleted when
//! that consumer adopted NMP's own publish queue.
//!
//! An app that wants NMP to SIGN without publishing already has a door:
//! [`Engine::sign_event`](crate::Engine::sign_event) returns the signed
//! event. That is a different question from routing, and it is answered
//! elsewhere rather than duplicated here.
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

    /// Publish one event into every retained group, through the ONE publish
    /// door.
    ///
    /// The whole door. One `h` row per retained id is appended BEFORE the
    /// stamp/sign step so every row is inside the bytes that get signed, the
    /// route is [`WriteRouting::Explicit`] over the scope's whole host set,
    /// and the author is an exact frozen [`PublicKey`] rather than a reactive
    /// selector (#878). The app names neither a relay nor an `h` row, and
    /// never holds the intent: NMP contextualizes, NMP signs, NMP publishes,
    /// and the app reads its own write back through the subscription it
    /// already holds.
    ///
    /// A draft that already carries an `h` row is refused whichever value it
    /// holds, because that row belongs to the retained scope and not to the
    /// caller.
    ///
    /// Returns the ordinary [`ReceiptStream`] every other write returns,
    /// store-issued [`ReceiptId`](crate::ReceiptId) included.
    pub fn publish(
        &self,
        engine: &Engine,
        author: PublicKey,
        builder: EventBuilder,
    ) -> Result<ReceiptStream, GroupPublishError> {
        let contextualized = nmp_nip29::contextualize(&self.ids, builder)?;
        let intent = self.mint(
            WritePayload::Event(contextualized),
            Identity::Explicit(author),
        );
        engine.publish(intent).map_err(GroupPublishError::Engine)
    }

    /// The one shape a group write has. `Explicit(every host)` comes from the
    /// scope the app named once: no caller supplies it and no engine derives
    /// it. The `h` rows carry GROUP IDS, never relays, so the hosts are not
    /// derivable from the event and no resolver could ever compute them.
    ///
    /// Private, and the intent never leaves this function: a
    /// [`WriteIntent`] carries no derives at all -- not `Clone`, not `Debug`,
    /// not `Serialize` -- so an app holding one could not persist it across a
    /// restart, batch it, or inspect it. Handing one out would buy nothing
    /// and would be a second write lifecycle to keep honest.
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
    use nostr::{Keys, Kind, Tag};

    fn host(n: u16) -> RelayUrl {
        RelayUrl::parse(&format!("wss://host-{n}.example.com")).expect("a well-formed host")
    }

    fn engine() -> Engine {
        Engine::new(crate::config::EngineConfig::default()).expect("a temporary Redb engine builds")
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

    /// A write context over no group is refused where the value is FORMED, so
    /// no `Groups` exists on that path at all -- the invalid state is
    /// unconstructible rather than validated later, exactly as an empty relay
    /// set is.
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
    /// same rooms differently hold the SAME value and compose the same bytes.
    #[test]
    fn duplicate_and_unsorted_ids_canonicalize_to_one_set() {
        let scope = nip29::on([host(1)]).expect("a nonempty scope");
        assert_eq!(
            scope.groups(["b", "a", "b"]).expect("two rooms"),
            scope.groups(["a", "b"]).expect("two rooms")
        );
    }

    /// The `h` row belongs to the retained scope at this arity too, and the
    /// refusal happens BEFORE anything is signed, routed or accepted -- no
    /// receipt stream is returned at all, which is what "the write never
    /// reached the door" means.
    #[test]
    fn a_caller_supplied_context_is_refused_before_any_several_group_write_is_accepted() {
        let engine = engine();
        assert_eq!(
            rooms()
                .publish(
                    &engine,
                    author(),
                    nmp_grammar::EventBuilder::new(Kind::from(30315u16))
                        .tag(Tag::parse(["h", "darkroom"]).unwrap()),
                )
                .err(),
            Some(GroupPublishError::Context(
                GroupContextError::CallerSuppliedContext
            ))
        );
        engine.shutdown();
    }

    /// A several-group write is an ORDINARY tracked write: it reaches the one
    /// publish door and comes back with the store-issued receipt id every
    /// other write returns. There is no group-shaped receipt and no second
    /// write lifecycle.
    #[test]
    fn a_several_group_write_is_an_ordinary_tracked_write() {
        let engine = engine();
        let receipt = rooms()
            .publish(
                &engine,
                author(),
                nmp_grammar::EventBuilder::new(Kind::from(30315u16))
                    .tag(Tag::parse(["d", "status"]).unwrap()),
            )
            .expect("the publish door accepts a several-group write");
        let queued = engine
            .publish_queue(None, u8::MAX)
            .expect("the queue reads back");
        assert!(
            queued.iter().any(|entry| entry.receipt_id == receipt.id),
            "the id a several-group publication returns must name a real queue entry"
        );
        engine.shutdown();
    }

    /// The app never holds a `WriteIntent`, so the door's own composition is
    /// proved where it is decided: the contextualizer this door calls, over
    /// the exact retained set, is what puts one `h` row per room inside the
    /// signed bytes. `nmp-nip29`'s
    /// `a_draft_for_several_groups_carries_one_h_row_per_group` pins the row
    /// list; this pins that THIS door composes with the whole retained set
    /// and not a subset of it.
    #[test]
    fn the_door_contextualizes_with_the_whole_retained_set() {
        let rooms = rooms();
        let composed = nmp_nip29::contextualize(
            &rooms.ids,
            nmp_grammar::EventBuilder::new(Kind::from(30315u16))
                .tag(Tag::parse(["d", "status"]).unwrap()),
        )
        .expect("a plain draft contextualizes for several groups");
        assert_eq!(
            composed
                .tags
                .iter()
                .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("h"))
                .map(|tag| tag.as_slice()[1].clone())
                .collect::<Vec<String>>(),
            vec!["darkroom".to_string(), "photographers".to_string()],
            "one h row per room, so the one replaceable event renders in both"
        );
        assert_eq!(
            composed.tags[0].as_slice(),
            &["d".to_string(), "status".to_string()],
            "the app's own addressable coordinate survives ahead of the appended rows"
        );
    }

    /// THE reason a multi-`h` write is the only correct shape, proved by
    /// construction rather than asserted: two per-room copies of the same
    /// status share one addressable coordinate `(kind, author, d)`, so a
    /// relay applying NIP-01 replacement keeps exactly one of them. If this
    /// ever observes two DIFFERENT coordinates there was nothing to fix.
    #[test]
    fn publishing_once_per_room_would_share_one_replaceable_coordinate() {
        let me = author();
        let draft = || {
            nmp_grammar::EventBuilder::new(Kind::from(30315u16))
                .tag(Tag::parse(["d", "status"]).unwrap())
        };
        let coordinate = |group_id: &str| {
            let composed =
                nmp_nip29::contextualize(&BTreeSet::from([group_id.to_string()]), draft())
                    .expect("a plain draft contextualizes");
            (
                composed.kind,
                me,
                composed
                    .tags
                    .iter()
                    .find(|tag| tag.as_slice().first().map(String::as_str) == Some("d"))
                    .map(|tag| tag.as_slice()[1].clone()),
            )
        };
        assert_eq!(
            coordinate("darkroom"),
            coordinate("photographers"),
            "NOTHING TO OBSERVE unless the two per-room copies really do collide"
        );
    }

    /// The route is the scope's whole host set, minted by the door: the write
    /// reaches every host the app named once and no other.
    #[test]
    fn a_several_group_write_routes_to_every_host_in_the_scope() {
        let hosts = BTreeSet::from([host(1), host(2)]);
        let rooms = Groups::new(hosts.clone(), BTreeSet::from(["darkroom".to_string()]))
            .expect("a nonempty group set");
        match rooms
            .mint(
                WritePayload::Event(nmp_grammar::EventBuilder::new(Kind::from(9u16))),
                Identity::Explicit(author()),
            )
            .routing
        {
            WriteRouting::Explicit(relays) => {
                assert_eq!(relays, vec![host(1), host(2)]);
            }
            WriteRouting::Auto => panic!("a group write is never Auto"),
        }
    }
}
