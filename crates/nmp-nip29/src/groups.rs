//! [`Groups`] -- the several groups one write belongs to (#1281).
//!
//! [`Group`](crate::Group) is a room: an identity an app reads, watches,
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
//! [`Engine::sign_event`](nmp::Engine::sign_event) returns the signed
//! event. That is a different question from routing, and it is answered
//! elsewhere rather than duplicated here.
//!
//! # It is the same door, at a larger arity
//!
//! There is no second mechanism here. [`Group`](crate::Group)'s whole write
//! half IS a one-element `Groups`: `Group::intent` builds one and calls
//! [`Groups::intent`], and `crate::contextualize` takes a set at every
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
//! Nonempty by construction, the same shape [`nip29::on`](crate::on) has for
//! relays: [`RelayScope::groups`](crate::RelayScope::groups) is the only way
//! to make one and it refuses an empty set, because an event with no `h` row
//! is not in a group at all.

use std::collections::BTreeSet;

use nmp_grammar::{EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};
use nostr::{PublicKey, RelayUrl};

use crate::group::GroupPublishError;
use crate::GroupContextError;
use nmp::{Engine, ReceiptStream};

/// The groups one write belongs to, on the relays their scope named.
///
/// Retains both privately, exactly as [`Group`](crate::Group) does: no host
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
    /// Fallible for the one reason [`nip29::on`](crate::on) is: the set is
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
        let contextualized = crate::contextualize(&self.ids, builder)?;
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
        }
    }
}

