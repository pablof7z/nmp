//! [`Group`] -- one group id within a [`RelayScope`](crate::RelayScope)
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
//! # [`Group::publish`] is the write half, and it is the whole of it
//!
//! One door (#1292). It contextualizes the draft, mints the ordinary opaque
//! [`WriteIntent`] -- `h` row appended before signing, route minted from the
//! retained scope, author frozen -- and hands it to
//! [`Engine::publish`]. Every named operation is that same call with
//! a composed builder; there is no second contextualization, no
//! group-shaped receipt and no group-shaped retry, and no surface hands an
//! unpublished intent back to an app. The returned [`ReceiptStream`] is the
//! ordinary one every other write returns, store-issued
//! [`ReceiptId`](nmp::ReceiptId) included (#1244).
//!
//! An app that needs a signed event without publishing it asks the engine
//! for exactly that: [`Engine::sign_event`] creates no write intent, pending
//! row, receipt, delivery lane, relay plan or publication, and hands back the
//! signed event. That is a signer door, not a second write door, and it is
//! why this module needs none.

use std::collections::BTreeSet;

use nmp_grammar::{EventBuilder, Filter, Identity, WriteIntent, WritePayload, WriteRouting};
use nostr::{Event, EventId, PublicKey, RelayUrl};

use crate::read::{self, GroupReadError};
use crate::record_observation::{GroupObservation, GroupObserveError};
use crate::GroupContextError;
use crate::GroupMetadataEdit;
use crate::GroupRecord;
use crate::{GroupUser, GroupUsersError};
use nmp::{Engine, EngineError, LiveQuery, ReceiptStream};

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
    /// A multi-user moderation operation named nobody or assigned one user
    /// conflicting roles. No write was accepted.
    Users(GroupUsersError),
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
            Self::Users(error) => write!(f, "{error}"),
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
    /// [`RelayScope::records_branches`](crate::RelayScope::records_branches):
    /// the per-branch scoping property must be assertable for a MULTI-host
    /// group independently of how branches are aggregated.
    pub(crate) fn read_branches(
        &self,
        selection: Filter,
    ) -> Result<Vec<nmp_grammar::Demand>, GroupContextError> {
        self.hosts
            .iter()
            .map(|host| crate::group_demand_at(host, &self.id, selection.clone()))
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
        let predicate: crate::GroupPredicate = crate::any_of(this_id).into();
        let branches = self
            .hosts
            .iter()
            .map(|host| crate::group_records_at(host, &records, predicate.lower_at(host), None))
            .collect();
        crate::record_observation::observe(
            engine,
            self.hosts.clone(),
            BTreeSet::from([self.id.clone()]),
            branches,
        )
    }

    /// Ask whether an already-signed event belongs to this group, without
    /// building a write out of it.
    pub fn validate_context(&self, event: &Event) -> Result<(), GroupContextError> {
        crate::validate_context(&BTreeSet::from([self.id.clone()]), event)
    }

    /// Publish an unsigned draft into the group, as `author`. The group's
    /// ONE write door (#1292).
    ///
    /// The group appends exactly one `["h", group_id]` row BEFORE the
    /// stamp/sign step, so the context tag is inside the bytes that get
    /// signed, and mints [`WriteRouting::Explicit`] over every host in the
    /// scope. The refusals -- a caller-supplied `h`, a caller-supplied
    /// timeline -- are decided before the intent reaches the engine, which is
    /// where a caller error belongs.
    ///
    /// `author` is an exact decoded [`PublicKey`], never a reactive selector:
    /// a semantic group write freezes who is writing at composition time
    /// rather than resolving it later against whoever happens to be active
    /// (#878). Recurrent identity remains entirely valid on the READ side.
    ///
    /// The returned [`ReceiptStream`] is the ordinary one every other write
    /// returns, store-issued [`ReceiptId`](nmp::ReceiptId) included
    /// (#1244).
    ///
    /// Kind-blind: no kind is privileged, refused, or read.
    pub fn publish(
        &self,
        engine: &Engine,
        author: PublicKey,
        builder: EventBuilder,
    ) -> Result<ReceiptStream, GroupPublishError> {
        let contextualized = crate::contextualize(&BTreeSet::from([self.id.clone()]), builder)?;
        let intent = self.mint(
            WritePayload::Event(contextualized),
            Identity::Explicit(author),
        );
        engine.publish(intent).map_err(GroupPublishError::Engine)
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
        self.publish(engine, author, crate::join_request(invite_code))
    }

    /// kind:9022 -- leave.
    pub fn leave_request(
        &self,
        engine: &Engine,
        author: PublicKey,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, crate::leave_request())
    }

    /// kind:9000 -- add several members in one event, optionally with a role
    /// per member.
    pub fn add_users(
        &self,
        engine: &Engine,
        author: PublicKey,
        users: impl IntoIterator<Item = GroupUser>,
    ) -> Result<ReceiptStream, GroupPublishError> {
        let builder = crate::add_users(users).map_err(GroupPublishError::Users)?;
        self.publish(engine, author, builder)
    }

    /// kind:9001 -- remove several members in one event.
    pub fn remove_users(
        &self,
        engine: &Engine,
        author: PublicKey,
        pubkeys: impl IntoIterator<Item = PublicKey>,
    ) -> Result<ReceiptStream, GroupPublishError> {
        let builder = crate::remove_users(pubkeys).map_err(GroupPublishError::Users)?;
        self.publish(engine, author, builder)
    }

    /// kind:9002 -- state part of the group's metadata (#1282).
    ///
    /// Composes NIP-29's own 9002 rows and invents none: `name`, `about` and
    /// `picture`, plus the `public`/`private` and `open`/`closed` markers that
    /// decide who may read the group and whether join requests are honoured.
    /// An omitted field emits no tag at all, so it is left untouched rather
    /// than cleared -- see [`GroupMetadataEdit`] for the rows deliberately not
    /// composed, and why.
    pub fn edit_metadata(
        &self,
        engine: &Engine,
        author: PublicKey,
        edit: GroupMetadataEdit,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, crate::edit_metadata(edit))
    }

    /// kind:9005 -- delete one group-hosted event.
    pub fn delete_event(
        &self,
        engine: &Engine,
        author: PublicKey,
        event_id: EventId,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, crate::delete_event(event_id))
    }

    /// kind:9007 -- create the group at its hosts, optionally as a SUBGROUP
    /// of one that already exists there (#1301).
    ///
    /// `parent` is the parent's group id -- the same relay-scoped string the
    /// scope's group door takes, never an `naddr` and never a key. `None`
    /// creates a root group and composes no row at all.
    ///
    /// The relationship is stated HERE and not on
    /// [`edit_metadata`](Self::edit_metadata) on purpose: NIP-29's `Subgroups`
    /// section puts parenting on kind:9002, and the only relay that implements
    /// subgroups reads `parent` on the kind:9007 create -- validating there
    /// that the parent exists and that the signer administers it -- while
    /// ignoring the row entirely on a kind:9002.
    /// [`crate::create_group`] records the probe.
    pub fn create_group(
        &self,
        engine: &Engine,
        author: PublicKey,
        parent: Option<&str>,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, crate::create_group(parent))
    }

    /// kind:9008 -- delete the group from its hosts.
    pub fn delete_group(
        &self,
        engine: &Engine,
        author: PublicKey,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, crate::delete_group())
    }

    /// kind:9009 -- mint an invite code redeemable by
    /// [`join_request`](Self::join_request).
    pub fn create_invite(
        &self,
        engine: &Engine,
        author: PublicKey,
        code: &str,
    ) -> Result<ReceiptStream, GroupPublishError> {
        self.publish(engine, author, crate::create_invite(code))
    }

    /// The one shape a group write has. `Explicit(every host)` is minted
    /// HERE, from the scope the app named once: no caller supplies it and no
    /// engine derives it. The `h` tag carries the GROUP ID, never a relay, so
    /// the hosts are not derivable from the event and no resolver could ever
    /// compute them.
    ///
    fn mint(&self, payload: WritePayload, identity: Identity) -> WriteIntent {
        WriteIntent {
            payload,
            routing: WriteRouting::Explicit(self.hosts.iter().cloned().collect()),
            identity,
        }
    }
}

