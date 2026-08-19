//! A room: timeline pinned to its host relay, membership as live state,
//! posting into it, and admin.
//!
//! ## The verdict, up front
//!
//! This is the best-served surface in the app, by a wide margin. `nmp-nip29`
//! gives a `RelayScope` -> `Group` with `read`, `observe`, `publish`,
//! `join_request`, `leave_request`, `add_users`, `remove_users`,
//! `edit_metadata`, `delete_event`, `create_invite` -- and `Group::publish`
//! puts the `h` row inside the signed bytes and mints
//! `WriteRouting::Explicit(hosts)` in the same call, which is exactly the
//! "two contradicting routing authorities" problem solved at the right layer.
//! `GroupSnapshot` even carries `per_host` and `disagreements`, which is more
//! honesty than most clients would think to ask for.
//!
//! ## Where it still fights
//!
//! ### 1. The room timeline and the room's records are two different worlds
//!
//! `Group::observe(...)` returns `GroupObservation` -- rich snapshots,
//! `latest()` readable synchronously, availability folded. `Group::read(filter)`
//! returns a `LiveQuery` you hand to `Engine::observe` -- raw `RowDelta`s, no
//! order, no availability, fold it yourself. The messages in the room go
//! through the second one. So one screen runs two observations with two
//! delivery shapes, two error types, and two lifetimes, and the app joins them.
//!
//! ### 2. `GroupObservation::next()` is `async`; `Subscription::recv()` blocks
//!
//! There is no blocking room observation and no async-free way to await one.
//! `GroupObservation` has `latest()`, so an app CAN poll it -- but nothing
//! wakes the app to poll, so it either spins or brings an executor.
//! [`Room::open`] takes a `tokio::runtime::Handle` for exactly this reason,
//! and that is a dependency `nmp` does not give an app any way to obtain: the
//! engine's own is `Engine::adapter_runtime()`, `#[doc(hidden)]`, documented as
//! "Hidden mechanism, not an app scheduling API".
//!
//! ### 3. Composing a reaction to a room message
//!
//! The brief's hard case. It works, and it works because `Group::publish` is
//! kind-blind: `group.publish(&engine, author, nmp_nip25::react(&event, hint,
//! Reaction::Like))` puts the `h` row on the kind:7 and routes it to the host.
//! Two things about that call site:
//!
//! - `nmp_nip25::react` wants `&Event` and the timeline holds `Row`, so the
//!   user's own pending message cannot be reacted to (see `composer::react`).
//! - The relay hint passed to `react` comes from `Row::sources()`, which for a
//!   NIP-29 message is the host relay -- correct here, and correct for the
//!   wrong reason: it is "first source in sorted order", documented as a
//!   placeholder until #1378 decides a real policy. In a room with one host it
//!   is right by accident.
//!
//! ### 4. Membership is a `Vec<ListedSubject>`, and a predicate is a scan
//!
//! "Is this person a member?" is `snapshot.members.iter().any(...)`. For an
//! admin screen listing 500 members and asking per row, that is O(n) per row.
//! `nmp_nip29::member_list_includes(binding)` exists and is a QUERY predicate
//! -- it selects groups whose member list includes someone, which is the
//! inverse question.

use std::collections::BTreeSet;

use nmp::{Engine, Filter, PublicKey, ReceiptStream, RelayUrl, Row};
use nmp_nip29::{
    Group, GroupObservation, GroupPublishError, GroupRecord, GroupSnapshot, RelayScopeError,
};

use crate::rows::RowTable;

/// One open room screen: the records observation and the message timeline.
pub struct Room {
    group: Group,
    hosts: Vec<RelayUrl>,
    id: String,
    records: GroupObservation,
    timeline: nmp::Subscription,
    table: RowTable,
}

impl Room {
    /// Open a room.
    ///
    /// `runtime` is the app's own executor, needed only to drive
    /// `GroupObservation::next()`. Passing it in is this app admitting it could
    /// not get one from NMP.
    pub fn open(
        engine: &Engine,
        hosts: impl IntoIterator<Item = RelayUrl>,
        id: impl Into<String>,
        message_kinds: impl IntoIterator<Item = u16>,
    ) -> Result<Self, RoomError> {
        let id = id.into();
        let hosts: Vec<RelayUrl> = hosts.into_iter().collect();
        let group = nmp_nip29::group(hosts.clone(), id.clone()).map_err(RoomError::Scope)?;
        let records = group
            .observe(
                engine,
                [
                    GroupRecord::Metadata,
                    GroupRecord::Admins,
                    GroupRecord::Members,
                ],
            )
            .map_err(|error| RoomError::Records(error.to_string()))?;
        let selection = Filter {
            kinds: Some(message_kinds.into_iter().collect::<BTreeSet<u16>>()),
            ..Filter::default()
        };
        let query = group
            .read(selection)
            .map_err(|error| RoomError::Read(error.to_string()))?;
        let timeline = engine.observe(query, None).map_err(RoomError::Engine)?;
        Ok(Self {
            group,
            hosts,
            id,
            records,
            timeline,
            table: RowTable::new(),
        })
    }

    /// The room's own relay-signed records as of now. Synchronous, no await.
    /// `nmp-nip02`'s follow observation has no equivalent, which is the
    /// inconsistency `people::FollowButton` pays for.
    #[must_use]
    pub fn snapshot(&self) -> Option<GroupSnapshot> {
        self.records.latest().into_iter().next()
    }

    /// Pump the message timeline.
    pub fn poll_timeline(&mut self, timeout: std::time::Duration) -> Option<nmp::Frame> {
        let frame = self.timeline.recv_timeout(timeout).ok()?;
        self.table.apply(&frame);
        Some(frame)
    }

    /// Await the next records delivery. `async`, unavoidably.
    pub async fn next_records(&self) -> Option<Vec<GroupSnapshot>> {
        self.records.next().await.ok().flatten()
    }

    #[must_use]
    pub fn table(&self) -> &RowTable {
        &self.table
    }

    #[must_use]
    pub fn hosts(&self) -> &[RelayUrl] {
        &self.hosts
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Membership predicate, by scan.
    #[must_use]
    pub fn is_member(&self, who: PublicKey) -> Option<bool> {
        let snapshot = self.snapshot()?;
        Some(snapshot.members.iter().any(|listed| listed.pubkey == who))
    }

    #[must_use]
    pub fn is_admin(&self, who: PublicKey) -> Option<bool> {
        let snapshot = self.snapshot()?;
        Some(snapshot.admins.iter().any(|listed| listed.pubkey == who))
    }

    /// Post a message into the room. The `h` row and the host pinning are the
    /// group's, not the app's.
    pub fn post(
        &self,
        engine: &Engine,
        author: PublicKey,
        kind: u16,
        text: &str,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.group.publish(
            engine,
            author,
            nmp::EventBuilder::new(nmp::Kind::from(kind)).content(text),
        )
    }

    /// React to a room message: a kind:7 that must carry the `h` row AND the
    /// host constraint. The composed case the whole app was chosen to exercise.
    pub fn react(
        &self,
        engine: &Engine,
        author: PublicKey,
        target: &Row,
    ) -> Result<ReceiptStream, RoomReactError> {
        let event = target
            .signed_event()
            .ok_or(RoomReactError::TargetNotSigned)?;
        let hint = target.sources().iter().next().cloned();
        self.group
            .publish(
                engine,
                author,
                nmp_nip25::react(&event, hint, nmp_nip25::Reaction::Like),
            )
            .map_err(RoomReactError::Publish)
    }

    /// Reply to a room message. `nmp::reply_to` builds the NIP-10 rows, the
    /// group adds the `h`. Two independent tagging authorities that compose
    /// without either knowing about the other -- this is the design working.
    pub fn reply(
        &self,
        engine: &Engine,
        author: PublicKey,
        parent: &Row,
        text: &str,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.group
            .publish(engine, author, nmp::reply_to(parent).content(text))
    }

    pub fn join(
        &self,
        engine: &Engine,
        author: PublicKey,
        invite: Option<&str>,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.group.join_request(engine, author, invite)
    }

    pub fn leave(
        &self,
        engine: &Engine,
        author: PublicKey,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.group.leave_request(engine, author)
    }

    /// Admin: add members.
    pub fn add_users(
        &self,
        engine: &Engine,
        author: PublicKey,
        users: impl IntoIterator<Item = nmp_nip29::GroupUser>,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.group.add_users(engine, author, users)
    }

    /// Admin: remove members.
    pub fn remove_users(
        &self,
        engine: &Engine,
        author: PublicKey,
        users: impl IntoIterator<Item = PublicKey>,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.group.remove_users(engine, author, users)
    }
}

/// The rooms list: every room the current account is a member of, plus any the
/// app pins.
///
/// This one is genuinely five lines, and it is the only surface in this app
/// where the "wanted" and the "written" call sites are the same text.
pub fn rooms_list(
    engine: &Engine,
    hosts: impl IntoIterator<Item = RelayUrl>,
) -> Result<GroupObservation, RoomError> {
    nmp_nip29::on(hosts)
        .map_err(RoomError::Scope)?
        .observe(
            engine,
            nmp_nip29::member_list_includes(nmp::Binding::Reactive(
                nmp::IdentityField::ActivePubkey,
            )),
            [GroupRecord::Metadata, GroupRecord::Members],
            Some(250),
        )
        .map_err(|error| RoomError::Records(error.to_string()))
}

#[derive(Debug)]
pub enum RoomError {
    Scope(RelayScopeError),
    Records(String),
    Read(String),
    Engine(nmp::EngineError),
}

impl std::fmt::Display for RoomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Scope(error) => write!(f, "room scope: {error}"),
            Self::Records(reason) => write!(f, "room records: {reason}"),
            Self::Read(reason) => write!(f, "room read: {reason}"),
            Self::Engine(error) => write!(f, "room observation: {error}"),
        }
    }
}

impl std::error::Error for RoomError {}

#[derive(Debug)]
pub enum RoomReactError {
    TargetNotSigned,
    Publish(GroupPublishError),
}

impl std::fmt::Display for RoomReactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TargetNotSigned => f.write_str("the room message is not signed yet"),
            Self::Publish(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for RoomReactError {}
