//! Native projection of `nmp_nip29` -- the app-facing NIP-29 door (#1033,
//! Lane A of the #1033 FFI projection).
//!
//! Two objects, same narrowing the direct-Rust door uses:
//!
//! ```text
//! let scope = FfiRelayScope.on([hostA, hostB])   // which relays
//! let group = scope.group("photographers")       // narrowed to one group
//! ```
//!
//! [`FfiRelayScope`] wraps [`nmp_nip29::RelayScope`] and [`FfiGroup`] wraps
//! [`nmp_nip29::Group`] -- both opaque UniFFI objects (the same idiom
//! [`crate::blossom::FfiBlossomAuthorization`] uses for a proven Rust value
//! carried across the boundary), never a second mirrored copy of NIP-29's
//! own vocabulary. Neither type exposes its retained hosts or group id back
//! out: exactly like the Rust door, there is no spelling for composing an
//! event under one group and routing it as though it came from another.
//!
//! [`FfiGroupPredicate`] and [`FfiGroupIds`] wrap
//! [`nmp_nip29::GroupPredicate`] and [`nmp_nip29::GroupIds`] the same way.
//! They stay opaque for the same reason `FfiBinding::Derived`/`FfiBinding::
//! SetOp` are UniFFI objects rather than records (see `types.rs`'s own doc):
//! a caller composes them with [`member_list_includes`]/[`admin_list_includes`]/
//! [`any_of`]/[`groups_whose_record_matches`] and
//! [`FfiGroupIds::union`]/[`intersect`](FfiGroupIds::intersect)/[`minus`](FfiGroupIds::minus),
//! then hands the result to [`FfiRelayScope::observe_records`] -- they are
//! never inspected.
//!
//! The two-object split carries the Rust door's refusal ACROSS the boundary
//! rather than restating it in prose. Set algebra lives on [`FfiGroupIds`]
//! and on nothing else, so `all().minus(...)` is unspellable in Swift and
//! Kotlin exactly as it is in Rust: Nostr filters have no negation, and
//! "everything except X" could only be honoured by asking the relay for
//! everything and hiding rows after delivery.
//!
//! Deliberately absent, same as before #1033: a fixed group-content kind
//! catalog and a kind:9 composer. NIP-29 owns neither; C7 and client
//! notification policy remain independently optional (#838). Also absent:
//! any second projection of a Simple-groups entry. The observational value
//! lives in this NIP-29 feature family, and a caller wanting to browse a group
//! hands its `host_relay`/`group_id` fields to [`FfiRelayScope::on`]/
//! [`FfiRelayScope::group`] itself.

use std::sync::Arc;

use nmp_nip29::{
    self as nip29, Group, GroupAvailability, GroupIds, GroupMetadata, GroupMetadataEdit,
    GroupObservation, GroupPredicate, GroupRecord, GroupSnapshot, JoinAccess, ListedRecord,
    ListedSubject, ReadAccess, RelayScope,
};
use nostr::RelayUrl;

use crate::convert::{
    event_builder_from_ffi, filter_from_ffi, group_ids_binding_from_ffi, live_query_to_ffi,
    parse_event_id, parse_pubkey, signed_event_from_ffi, subjects_binding_from_ffi, FfiError,
};
use crate::facade::{NmpEngine, NmpReceiptStream};
use crate::types::{FfiBinding, FfiEventBuilder, FfiFilter, FfiLiveQuery, FfiSignedEvent};

/// One user in a kind:9000 add-users moderation event.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiGroupUser {
    pub pubkey: String,
    pub role: Option<String>,
}

fn parse_host(host: String) -> Result<RelayUrl, FfiError> {
    RelayUrl::parse(&host).map_err(|_| FfiError::InvalidRelayUrl { got: host })
}

/// The relays a group lives on -- named once, retained privately inside the
/// opaque handle, and never asked for again (`nmp_nip29::RelayScope`
/// mirror). `hosts` crosses the boundary as raw strings, unlike the
/// direct-Rust `on`'s `RelayUrl`s: fallibility (an empty set, or a host that
/// does not parse) is restored HERE, because the boundary widens from one
/// host to a caller-supplied set and a set can be empty where a single host
/// could not be.
#[derive(Debug, uniffi::Object)]
pub struct FfiRelayScope {
    inner: RelayScope,
}

#[uniffi::export]
impl FfiRelayScope {
    /// Name the relays a NIP-29 group lives on (`nmp_nip29::on` mirror).
    /// Each host is parsed with the same [`FfiError::InvalidRelayUrl`] rule
    /// every other relay-URL input in this crate uses; an empty set is
    /// [`FfiError::EmptyRelayScope`] -- a group must be hosted somewhere.
    #[uniffi::constructor]
    pub fn on(hosts: Vec<String>) -> Result<Arc<Self>, FfiError> {
        let hosts = hosts
            .into_iter()
            .map(parse_host)
            .collect::<Result<Vec<_>, _>>()?;
        let inner = nip29::on(hosts)?;
        Ok(Arc::new(Self { inner }))
    }

    /// Narrow to one group id, keeping the same hosts
    /// (`nmp_nip29::RelayScope::group` mirror). Contacts nothing.
    pub fn group(&self, group_id: String) -> Arc<FfiGroup> {
        Arc::new(FfiGroup {
            inner: self.inner.group(group_id),
        })
    }

    /// Narrow to the SEVERAL groups one write belongs to, keeping the same
    /// hosts (`nmp_nip29::RelayScope::groups` mirror, #1281).
    ///
    /// The write-only sibling of [`Self::group`], for the one event shape a
    /// single group id cannot express: a kind:30315 session status is
    /// addressable at `(author, d=status)` and carries one `h` per room the
    /// session occupies, so publishing it once per room would make each copy
    /// replace the last. An empty set is [`FfiError::EmptyGroupSet`] -- an
    /// event with no `h` row is not in a group at all.
    pub fn groups(&self, group_ids: Vec<String>) -> Result<Arc<FfiGroups>, FfiError> {
        Ok(Arc::new(FfiGroups {
            inner: self.inner.groups(group_ids)?,
        }))
    }

    /// Watch the relay-signed records of every group matching `predicate`
    /// (`nmp_nip29::RelayScope::observe` mirror). One complete branch per
    /// host, folded into ONE ordinary engine subscription; each delivery is
    /// the complete set of [`FfiGroupSnapshot`]s for the groups currently
    /// matching. The app never sees a row delta and never walks a `p` row.
    ///
    /// `limit` is the ordinary NIP-01 `Filter::limit` and bounds EACH host's
    /// own branch, never the merged union: two hosts with `Some(250)` may
    /// deliver up to 500 snapshots, because each was asked for 250 of its
    /// own. `None` asks for whatever the relay chooses to answer with.
    pub fn observe_records(
        &self,
        engine: Arc<NmpEngine>,
        predicate: Arc<FfiGroupPredicate>,
        records: Vec<FfiGroupRecord>,
        limit: Option<u32>,
    ) -> Result<Arc<NmpGroupRecordsStream>, FfiError> {
        let observation = self.inner.observe(
            &engine.engine,
            predicate.inner.clone(),
            records.into_iter().map(GroupRecord::from),
            limit.map(|limit| limit as usize),
        )?;
        Ok(NmpGroupRecordsStream::new(observation))
    }
}

/// One NIP-29 group, on the relays its scope named (`nmp_nip29::Group`
/// mirror). An identity, not a subscription: constructing one (via
/// [`FfiRelayScope::group`]) contacts nothing. The same handle serves every
/// read and every write for a room's whole lifetime.
#[derive(Debug, uniffi::Object)]
pub struct FfiGroup {
    inner: Group,
}

#[uniffi::export]
impl FfiGroup {
    /// Mint the read declaration for an app-supplied selection
    /// (`nmp_nip29::Group::read` mirror). A selection that already
    /// constrains `#h` is refused with
    /// [`FfiError::GroupCallerSuppliedContextConstraint`] -- the retained
    /// group id is the sole semantic source of that row. Hand the result to
    /// `NmpEngine::observe`.
    pub fn read(&self, selection: FfiFilter) -> Result<FfiLiveQuery, FfiError> {
        let selection = filter_from_ffi(selection)?;
        let query = self.inner.read(selection)?;
        Ok(live_query_to_ffi(query))
    }

    /// Watch THIS group's own relay-signed records
    /// (`nmp_nip29::Group::observe` mirror). Each delivery carries exactly
    /// one [`FfiGroupSnapshot`] -- this group's -- from the first delivery
    /// onward, including before any record has arrived.
    ///
    /// This is not a second read door: it opens the ONE ordinary engine
    /// subscription over the ONE ordinary live query the group's hosts
    /// declare, and folds the deltas an app would otherwise fold by hand.
    pub fn observe_records(
        &self,
        engine: Arc<NmpEngine>,
        records: Vec<FfiGroupRecord>,
    ) -> Result<Arc<NmpGroupRecordsStream>, FfiError> {
        let observation = self
            .inner
            .observe(&engine.engine, records.into_iter().map(GroupRecord::from))?;
        Ok(NmpGroupRecordsStream::new(observation))
    }

    /// Ask whether an already-signed event belongs to this group, without
    /// building a write out of it (`nmp_nip29::Group::validate_context`
    /// mirror).
    pub fn validate_context(&self, event: FfiSignedEvent) -> Result<(), FfiError> {
        let event = signed_event_from_ffi(
            event.id,
            event.pubkey,
            event.created_at,
            event.kind,
            event.tags,
            event.content,
            event.sig,
        )?;
        self.inner.validate_context(&event)?;
        Ok(())
    }

    /// Publish an unsigned draft into the group, as `author`
    /// (`nmp_nip29::Group::publish` mirror) -- the group's ONE write door
    /// (#1292).
    ///
    /// The `h` row is appended before signing, the route is the scope's own
    /// hosts, and `author` is frozen as an exact decoded pubkey rather than
    /// the current-account selector (#878). Returns the ORDINARY
    /// [`NmpReceiptStream`], store-issued receipt id included.
    ///
    /// An app that needs a signed event WITHOUT publishing it asks the engine
    /// for exactly that: `NmpEngine::signEvent` creates no write intent,
    /// receipt or publication and hands back the signed event.
    pub fn publish(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        builder: FfiEventBuilder,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let builder = event_builder_from_ffi(builder)?;
        let receipts = self.inner.publish(&engine.engine, author, builder)?;
        Ok(NmpReceiptStream::new(engine.engine.clone(), receipts))
    }

    /// kind:9021 -- ask to join. Publishable with no subscription at all.
    pub fn join_request(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        invite_code: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let receipts = self
            .inner
            .join_request(&engine.engine, author, invite_code.as_deref())?;
        Ok(NmpReceiptStream::new(engine.engine.clone(), receipts))
    }

    /// kind:9022 -- leave.
    pub fn leave_request(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let receipts = self.inner.leave_request(&engine.engine, author)?;
        Ok(NmpReceiptStream::new(engine.engine.clone(), receipts))
    }

    /// kind:9000 -- add several members in one event, optionally with a role
    /// per member.
    pub fn add_users(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        users: Vec<FfiGroupUser>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let users = users
            .into_iter()
            .map(|user| {
                Ok(nmp_nip29::GroupUser::new(
                    parse_pubkey(&user.pubkey)?,
                    user.role,
                ))
            })
            .collect::<Result<Vec<_>, FfiError>>()?;
        let receipts = self.inner.add_users(&engine.engine, author, users)?;
        Ok(NmpReceiptStream::new(engine.engine.clone(), receipts))
    }

    /// kind:9001 -- remove several members in one event.
    pub fn remove_users(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        pubkeys: Vec<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let pubkeys = pubkeys
            .into_iter()
            .map(|pubkey| parse_pubkey(&pubkey))
            .collect::<Result<Vec<_>, _>>()?;
        let receipts = self.inner.remove_users(&engine.engine, author, pubkeys)?;
        Ok(NmpReceiptStream::new(engine.engine.clone(), receipts))
    }

    /// kind:9002 -- state part of the group's metadata
    /// (`nmp_nip29::Group::edit_metadata` mirror, #1282).
    ///
    /// Composes NIP-29's own 9002 rows and invents none: `name`, `about` and
    /// `picture`, plus the `public`/`private` and `open`/`closed` markers
    /// that decide who may read the group and whether join requests are
    /// honoured. An omitted field emits no tag, so it is left untouched
    /// rather than cleared.
    pub fn edit_metadata(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        edit: FfiGroupMetadataEdit,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let receipts = self
            .inner
            .edit_metadata(&engine.engine, author, edit.into())?;
        Ok(NmpReceiptStream::new(engine.engine.clone(), receipts))
    }

    /// kind:9005 -- delete one group-hosted event.
    pub fn delete_event(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        event_id: String,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let event_id = parse_event_id(&event_id)?;
        let receipts = self.inner.delete_event(&engine.engine, author, event_id)?;
        Ok(NmpReceiptStream::new(engine.engine.clone(), receipts))
    }

    /// kind:9007 -- create the group at its hosts, optionally as a SUBGROUP
    /// of one that already exists there (#1301).
    ///
    /// `parent` is the parent's group id -- a relay-scoped string, never an
    /// `naddr`. `None` creates a root group and composes no row at all. The
    /// relationship rides on the create and not on an edit; see
    /// `nmp_nip29::Group::create_group` for why.
    pub fn create_group(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        parent: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let receipts = self
            .inner
            .create_group(&engine.engine, author, parent.as_deref())?;
        Ok(NmpReceiptStream::new(engine.engine.clone(), receipts))
    }

    /// kind:9008 -- delete the group from its hosts.
    pub fn delete_group(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let receipts = self.inner.delete_group(&engine.engine, author)?;
        Ok(NmpReceiptStream::new(engine.engine.clone(), receipts))
    }

    /// kind:9009 -- mint an invite code redeemable by
    /// [`Self::join_request`].
    pub fn create_invite(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        code: String,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let receipts = self.inner.create_invite(&engine.engine, author, &code)?;
        Ok(NmpReceiptStream::new(engine.engine.clone(), receipts))
    }
}

/// The groups one write belongs to (`nmp_nip29::Groups` mirror, #1281),
/// built with [`FfiRelayScope::groups`].
///
/// A WRITE CONTEXT and nothing else. There is no read door, no records
/// observation and no named operation on it, because each of those is
/// per-group by definition -- a roster is one group's, and every 9000-9022
/// moderation action names one group. A write is the one thing that is
/// genuinely plural.
///
/// ONE method. NMP appends the `h` rows, NMP signs, NMP publishes. There is
/// deliberately no pre-signed spelling, no way to obtain a draft to sign
/// yourself, and no mint-without-publish door -- an app that wants NMP to
/// sign without publishing uses `NmpEngine::sign_event`.
///
/// Opaque for the same reason [`FfiGroup`] is: it yields back neither its
/// hosts nor its ids, so no layer handed one can reconstruct the authority
/// and route something elsewhere under it.
#[derive(Debug, uniffi::Object)]
pub struct FfiGroups {
    inner: nip29::Groups,
}

#[uniffi::export]
impl FfiGroups {
    /// Publish one event into every retained group, through the ONE publish
    /// door (`nmp_nip29::Groups::publish` mirror).
    ///
    /// The whole door: one `h` row per retained id appended before signing,
    /// the route minted from the scope's own hosts, an exact frozen author.
    /// The app names neither a relay nor an `h` row and never holds a write
    /// intent -- NMP contextualizes, signs and publishes.
    pub fn publish(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        builder: FfiEventBuilder,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let builder = event_builder_from_ffi(builder)?;
        let receipts = self.inner.publish(&engine.engine, author, builder)?;
        Ok(NmpReceiptStream::new(engine.engine.clone(), receipts))
    }
}

/// Who may READ a group's messages (`nmp_nip29::ReadAccess` mirror, #1282).
///
/// NIP-29 spells the restricted state `["private"]` on kind:39000 and
/// kind:9002; the reference relay's 9002 parser spells the permissive one
/// `["public"]`, which is the only way an edit can say "turn it back off".
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiReadAccess {
    /// `["public"]` -- anyone may read the group's messages.
    Public,
    /// `["private"]` -- only members may read the group's messages.
    Private,
}

impl From<FfiReadAccess> for ReadAccess {
    fn from(value: FfiReadAccess) -> Self {
        match value {
            FfiReadAccess::Public => Self::Public,
            FfiReadAccess::Private => Self::Private,
        }
    }
}

/// Whether JOIN REQUESTS are honoured (`nmp_nip29::JoinAccess` mirror,
/// #1282). Independent of [`FfiReadAccess`]: a group can be publicly readable
/// and still closed to new members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiJoinAccess {
    /// `["open"]` -- join requests are honoured.
    Open,
    /// `["closed"]` -- join requests are ignored.
    Closed,
}

impl From<FfiJoinAccess> for JoinAccess {
    fn from(value: FfiJoinAccess) -> Self {
        match value {
            FfiJoinAccess::Open => Self::Open,
            FfiJoinAccess::Closed => Self::Closed,
        }
    }
}

/// What one kind:9002 edit says about a group
/// (`nmp_nip29::GroupMetadataEdit` mirror, #1282).
///
/// Every field is optional: `None` leaves that row out of the draft
/// entirely, so it is not touched and never cleared. That is why the two
/// markers are two-valued enums rather than booleans -- "make it public" and
/// "do not decide" are different statements, and one boolean cannot make
/// both.
///
/// A record rather than an opaque object, unlike [`FfiGroup`]: there is
/// nothing retained here and nothing to keep out of a caller's reach. It is
/// an argument, and every field is meant to be spelled by the app.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct FfiGroupMetadataEdit {
    /// The `name` row -- the group's display name.
    #[uniffi(default = None)]
    pub name: Option<String>,
    /// The `about` row -- the group's description.
    #[uniffi(default = None)]
    pub about: Option<String>,
    /// The `picture` row. The tag NAME is NIP-29's; which URL goes in it is
    /// entirely the app's product policy.
    #[uniffi(default = None)]
    pub picture: Option<String>,
    /// Who may read the group's messages.
    #[uniffi(default = None)]
    pub read_access: Option<FfiReadAccess>,
    /// Whether join requests are honoured.
    #[uniffi(default = None)]
    pub join_access: Option<FfiJoinAccess>,
}

impl From<FfiGroupMetadataEdit> for GroupMetadataEdit {
    fn from(edit: FfiGroupMetadataEdit) -> Self {
        Self {
            name: edit.name,
            about: edit.about,
            picture: edit.picture,
            read_access: edit.read_access.map(ReadAccess::from),
            join_access: edit.join_access.map(JoinAccess::from),
        }
    }
}

/// Which groups an observation covers (`nmp_nip29::GroupPredicate` mirror).
/// Opaque by design -- see this module's own doc for why -- built with
/// [`Self::all`] or from an [`FfiGroupIds`] with [`Self::naming`], then handed
/// to [`FfiRelayScope::observe_records`].
#[derive(Debug, uniffi::Object)]
pub struct FfiGroupPredicate {
    inner: GroupPredicate,
}

#[uniffi::export]
impl FfiGroupPredicate {
    /// Every group the host advertises among the selected records
    /// (`nmp_nip29::all` mirror). The branch carries NO group-id row: this is
    /// the ABSENCE of a constraint, which is what makes a directory
    /// expressible -- the ids a directory wants are the answer, not the input.
    ///
    /// Unbounded by nature: bound it with `observe_records`'s own `limit`.
    /// Advertisement is not enumeration -- a group the host serves but
    /// publishes no kind:39000 for is invisible.
    #[uniffi::constructor]
    pub fn all() -> Arc<Self> {
        Arc::new(Self {
            inner: nip29::all(),
        })
    }

    /// Only the groups `ids` names (`From<GroupIds> for GroupPredicate`
    /// mirror).
    #[uniffi::constructor]
    pub fn naming(ids: Arc<FfiGroupIds>) -> Arc<Self> {
        Arc::new(Self {
            inner: ids.inner.clone().into(),
        })
    }
}

/// Where a set of NIP-29 group ids comes from (`nmp_nip29::GroupIds`
/// mirror). Opaque by design, built with [`member_list_includes`]/
/// [`admin_list_includes`]/[`any_of`]/[`groups_whose_record_matches`] and
/// composed with [`Self::union`]/[`Self::intersect`]/[`Self::minus`].
///
/// Whatever this resolves to becomes the `#d` value set of one relay filter,
/// and a filter carrying very many values may be refused or silently
/// truncated by that relay. Watching very many groups needs sharding across
/// several observations; NMP does not chunk behind the app's back, because a
/// silently-sharded observation would report availability for a plan the app
/// never declared.
#[derive(Debug, uniffi::Object)]
pub struct FfiGroupIds {
    inner: GroupIds,
}

#[uniffi::export]
impl FfiGroupIds {
    /// Groups named by this source OR by any of `others`.
    pub fn union(&self, others: Vec<Arc<FfiGroupIds>>) -> Arc<Self> {
        let inner = self
            .inner
            .clone()
            .union(others.iter().map(|other| other.inner.clone()));
        Arc::new(Self { inner })
    }

    /// Groups named by this source AND by all of `others`.
    pub fn intersect(&self, others: Vec<Arc<FfiGroupIds>>) -> Arc<Self> {
        let inner = self
            .inner
            .clone()
            .intersect(others.iter().map(|other| other.inner.clone()));
        Arc::new(Self { inner })
    }

    /// Groups named by this source and by none of `others`.
    pub fn minus(&self, others: Vec<Arc<FfiGroupIds>>) -> Arc<Self> {
        let inner = self
            .inner
            .clone()
            .minus(others.iter().map(|other| other.inner.clone()));
        Arc::new(Self { inner })
    }
}

/// Groups whose own relay-signed record matches `selection` at the branch
/// host (`nmp_nip29::groups_whose_record_matches` mirror) -- THE general
/// spelling, of which every other id source is a shorthand.
///
/// Refused when `selection` names no kind, or names a kind that is not one of
/// NIP-29's three relay-signed group records: this leaf is evaluated with
/// NIP-29's own pin, and a group host is authoritative for nothing else.
#[uniffi::export]
pub fn groups_whose_record_matches(selection: FfiFilter) -> Result<Arc<FfiGroupIds>, FfiError> {
    let selection = filter_from_ffi(selection)?;
    Ok(Arc::new(FfiGroupIds {
        inner: nip29::groups_whose_record_matches(selection)?,
    }))
}

/// Groups whose observed kind:39002 member-list evidence names `subjects`
/// (`nmp_nip29::member_list_includes` mirror). Inclusion is evidence,
/// never exact state -- absence is not evidence of non-membership.
///
/// Shorthand for [`groups_whose_record_matches`] over
/// `{ kinds:[39002], #p: subjects }`, and exactly equal to it.
#[uniffi::export]
pub fn member_list_includes(subjects: FfiBinding) -> Result<Arc<FfiGroupIds>, FfiError> {
    let subjects = subjects_binding_from_ffi(subjects)?;
    Ok(Arc::new(FfiGroupIds {
        inner: nip29::member_list_includes(subjects),
    }))
}

/// Groups whose observed kind:39001 admin-list evidence names `subjects`
/// (`nmp_nip29::admin_list_includes` mirror). Evidence-scoped exactly like
/// [`member_list_includes`].
#[uniffi::export]
pub fn admin_list_includes(subjects: FfiBinding) -> Result<Arc<FfiGroupIds>, FfiError> {
    let subjects = subjects_binding_from_ffi(subjects)?;
    Ok(Arc::new(FfiGroupIds {
        inner: nip29::admin_list_includes(subjects),
    }))
}

/// Which of NIP-29's three relay-signed group records an app is asking for
/// (`nmp_nip29::GroupRecord` mirror).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGroupRecord {
    /// kind:39000 -- the group's own metadata.
    Metadata,
    /// kind:39001 -- the optional, informative admin list.
    Admins,
    /// kind:39002 -- the optional, possibly partial member list.
    Members,
}

impl From<FfiGroupRecord> for GroupRecord {
    fn from(record: FfiGroupRecord) -> Self {
        match record {
            FfiGroupRecord::Metadata => Self::Metadata,
            FfiGroupRecord::Admins => Self::Admins,
            FfiGroupRecord::Members => Self::Members,
        }
    }
}

impl From<GroupRecord> for FfiGroupRecord {
    fn from(record: GroupRecord) -> Self {
        match record {
            GroupRecord::Metadata => Self::Metadata,
            GroupRecord::Admins => Self::Admins,
            GroupRecord::Members => Self::Members,
        }
    }
}

/// How much of what the app asked for has been established
/// (`nmp_nip29::GroupAvailability` mirror). Says nothing about whether the
/// records themselves are complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGroupAvailability {
    SourceUnavailable,
    Acquiring,
    CachedOnly,
    Ready,
}

/// One subject a relay-signed list names, and the hosts that named it
/// (`nmp_nip29::ListedSubject` mirror). `role` is absent when the relay
/// wrote none -- never defaulted.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiListedSubject {
    pub pubkey: String,
    pub role: Option<String>,
    pub hosts: Vec<String>,
}

/// One relay-signed list record (`nmp_nip29::ListedRecord` mirror).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiListedRecord {
    pub subjects: Vec<FfiListedSubject>,
    /// The record's own `created_at`. A DISPLAY fact -- never compared
    /// against a local clock to adjudicate anything.
    pub as_of: u64,
    pub event_id: String,
    pub host: String,
}

/// One relay-signed kind:39000 record (`nmp_nip29::GroupMetadata` mirror).
/// The three rows NIP-29 names are typed; `tags` carries the record's
/// complete row list verbatim, so a row NIP-29 core does not define (a
/// `parent`, say) needs no hand-parser on the native side.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiGroupMetadata {
    pub name: Option<String>,
    pub about: Option<String>,
    pub picture: Option<String>,
    pub tags: Vec<Vec<String>>,
    pub as_of: u64,
    pub event_id: String,
    pub host: String,
}

/// Exactly what one host signed, folded with nothing
/// (`nmp_nip29::HostRecords` mirror).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiHostRecords {
    pub host: String,
    pub metadata: Option<FfiGroupMetadata>,
    pub admins: Option<FfiListedRecord>,
    pub members: Option<FfiListedRecord>,
    pub availability: FfiGroupAvailability,
}

/// One group, as the hosts in the scope currently describe it
/// (`nmp_nip29::GroupSnapshot` mirror). A complete self-contained value,
/// never a patch on a previous one.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiGroupSnapshot {
    pub id: String,
    /// The whole winning host's record -- latest `created_at` wins, never a
    /// field-wise merge across hosts.
    pub metadata: Option<FfiGroupMetadata>,
    /// The union across hosts, each entry carrying the hosts that named it.
    pub admins: Vec<FfiListedSubject>,
    pub members: Vec<FfiListedSubject>,
    /// The minimum over every host in the scope.
    pub availability: FfiGroupAvailability,
    /// Exactly what each host that answered signed, in host order.
    pub per_host: Vec<FfiHostRecords>,
    /// The records the answering hosts do not agree on.
    pub disagreements: Vec<FfiGroupRecord>,
}

/// Pull-based group-records observation handle (`nmp_nip29::GroupObservation`
/// mirror). Each `next()` awaits the engine's waker-driven async row mailbox
/// and folds a complete self-contained snapshot set inline. `None` is the
/// terminal signal (the demand was withdrawn or the engine shut down);
/// `Drop`/`cancel` withdraw the observation.
#[derive(uniffi::Object)]
pub struct NmpGroupRecordsStream {
    inner: GroupObservation,
}

impl NmpGroupRecordsStream {
    fn new(inner: GroupObservation) -> Arc<Self> {
        Arc::new(Self { inner })
    }
}

#[uniffi::export]
impl NmpGroupRecordsStream {
    /// Await the next snapshot set, or `None` once the observation is
    /// withdrawn. A second concurrent `next()` is [`FfiError::ConcurrentNext`].
    pub async fn next(&self) -> Result<Option<Vec<FfiGroupSnapshot>>, FfiError> {
        match self.inner.next().await {
            Ok(Some(snapshots)) => Ok(Some(snapshots.iter().map(snapshot_to_ffi).collect())),
            Ok(None) => Ok(None),
            Err(_) => Err(FfiError::ConcurrentNext),
        }
    }

    /// Withdraw the observation now (idempotent; `Drop` does the same).
    pub fn cancel(&self) {
        self.inner.cancel();
    }
}

impl Drop for NmpGroupRecordsStream {
    fn drop(&mut self) {
        self.inner.cancel();
    }
}

fn availability_to_ffi(availability: GroupAvailability) -> FfiGroupAvailability {
    match availability {
        GroupAvailability::SourceUnavailable => FfiGroupAvailability::SourceUnavailable,
        GroupAvailability::Acquiring => FfiGroupAvailability::Acquiring,
        GroupAvailability::CachedOnly => FfiGroupAvailability::CachedOnly,
        GroupAvailability::Ready => FfiGroupAvailability::Ready,
    }
}

fn subject_to_ffi(subject: &ListedSubject) -> FfiListedSubject {
    FfiListedSubject {
        pubkey: subject.pubkey.to_hex(),
        role: subject.role.clone(),
        hosts: subject.hosts.iter().map(RelayUrl::to_string).collect(),
    }
}

fn listed_record_to_ffi(record: &ListedRecord) -> FfiListedRecord {
    FfiListedRecord {
        subjects: record.subjects.iter().map(subject_to_ffi).collect(),
        as_of: record.as_of.as_secs(),
        event_id: record.event_id.to_hex(),
        host: record.host.to_string(),
    }
}

fn metadata_to_ffi(metadata: &GroupMetadata) -> FfiGroupMetadata {
    FfiGroupMetadata {
        name: metadata.name.clone(),
        about: metadata.about.clone(),
        picture: metadata.picture.clone(),
        tags: metadata.tags.clone(),
        as_of: metadata.as_of.as_secs(),
        event_id: metadata.event_id.to_hex(),
        host: metadata.host.to_string(),
    }
}

pub(crate) fn snapshot_to_ffi(snapshot: &GroupSnapshot) -> FfiGroupSnapshot {
    FfiGroupSnapshot {
        id: snapshot.id.clone(),
        metadata: snapshot.metadata.as_ref().map(metadata_to_ffi),
        admins: snapshot.admins.iter().map(subject_to_ffi).collect(),
        members: snapshot.members.iter().map(subject_to_ffi).collect(),
        availability: availability_to_ffi(snapshot.availability),
        per_host: snapshot
            .per_host
            .iter()
            .map(|(host, records)| FfiHostRecords {
                host: host.to_string(),
                metadata: records.metadata.as_ref().map(metadata_to_ffi),
                admins: records.admins.as_ref().map(listed_record_to_ffi),
                members: records.members.as_ref().map(listed_record_to_ffi),
                availability: availability_to_ffi(records.availability),
            })
            .collect(),
        disagreements: snapshot
            .disagreements
            .iter()
            .copied()
            .map(FfiGroupRecord::from)
            .collect(),
    }
}

/// The groups `ids` names, whatever any list says about them
/// (`nmp_nip29::any_of` mirror).
///
/// `ids` is an ordinary [`FfiBinding`], which is the point: a literal set for
/// rooms an app already knows, and a derived binding for rooms it has to look
/// up. "Watch the groups named in my own kind:10009 simple-groups list" is
/// that derived case, and it stays reactive -- when the list changes, the
/// observation follows it, with no hand-extraction of ids and no second
/// observation. A derived binding keeps its OWN authority and is never
/// repinned to the group's hosts.
#[uniffi::export]
pub fn any_of(ids: FfiBinding) -> Result<Arc<FfiGroupIds>, FfiError> {
    let ids = group_ids_binding_from_ffi(ids)?;
    Ok(Arc::new(FfiGroupIds {
        inner: nip29::any_of(ids),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FfiAccessContext, FfiIdentityField, FfiSourceAuthority};

    fn host(n: u16) -> String {
        format!("wss://host-{n}.example.com")
    }

    #[test]
    fn on_rejects_an_empty_relay_set() {
        match FfiRelayScope::on(vec![]) {
            Err(FfiError::EmptyRelayScope) => {}
            other => panic!("expected EmptyRelayScope, got {other:?}"),
        }
    }

    #[test]
    fn on_rejects_an_unparseable_host() {
        match FfiRelayScope::on(vec!["not-a-url".to_string()]) {
            Err(FfiError::InvalidRelayUrl { got }) => assert_eq!(got, "not-a-url"),
            other => panic!("expected InvalidRelayUrl, got {other:?}"),
        }
    }

    /// A multi-host group read is ONE live query with one complete branch
    /// per host, each pinned to that host alone and scoped by `#h` -- the
    /// FFI mirror of `nmp_nip29::Group::read`'s own falsifier.
    #[test]
    fn a_group_read_is_one_branch_per_host_pinned_to_that_host() {
        let scope = FfiRelayScope::on(vec![host(1), host(2)]).expect("two hosts parse");
        let group = scope.group("photographers".to_string());
        let query = group
            .read(FfiFilter::default())
            .expect("a plain selection scopes");

        assert_eq!(query.branches.len(), 2);
        for (branch, expected_host) in query.branches.iter().zip([host(1), host(2)]) {
            assert_eq!(
                branch.source,
                FfiSourceAuthority::Pinned {
                    relays: vec![expected_host]
                }
            );
            assert_eq!(branch.access, FfiAccessContext::Public);
            assert_eq!(
                branch.selection.tags.get("h"),
                Some(&FfiBinding::Literal {
                    values: vec!["photographers".to_string()]
                })
            );
        }
        assert_eq!(query.aggregate_result_limit, None);
    }

    /// A read selection that already constrains `#h` is refused before any
    /// live query is formed -- the retained group id is the sole semantic
    /// source of that row.
    #[test]
    fn a_read_selection_naming_its_own_h_row_is_refused() {
        let scope = FfiRelayScope::on(vec![host(1)]).expect("one host parses");
        let group = scope.group("photographers".to_string());
        let selection = FfiFilter {
            tags: std::collections::HashMap::from([(
                "h".to_string(),
                FfiBinding::Literal {
                    values: vec!["elsewhere".to_string()],
                },
            )]),
            ..FfiFilter::default()
        };
        match group.read(selection) {
            Err(FfiError::GroupCallerSuppliedContextConstraint) => {}
            other => panic!("expected GroupCallerSuppliedContextConstraint, got {other:?}"),
        }
    }

    /// The composable predicate door: union/intersect/minus fold through
    /// the grammar's own set algebra, including the literal-id leaf, and a
    /// multi-host observation opens over every host.
    #[test]
    fn a_composed_predicate_observes_the_records_over_every_host() {
        let engine =
            NmpEngine::new(crate::facade::NmpEngineConfig::default(), None).expect("engine builds");
        let scope = FfiRelayScope::on(vec![host(1), host(2)]).expect("two hosts parse");
        let member = member_list_includes(FfiBinding::Reactive {
            field: FfiIdentityField::ActivePubkey,
        })
        .expect("a reactive subjects binding needs no hex validation");
        let admin = admin_list_includes(FfiBinding::Reactive {
            field: FfiIdentityField::ActivePubkey,
        })
        .expect("a reactive subjects binding needs no hex validation");
        let pinned = any_of(FfiBinding::Literal {
            values: vec!["photographers".to_string()],
        })
        .expect("a literal id set needs no hex validation");
        let predicate = FfiGroupPredicate::naming(member.union(vec![admin, pinned]));

        let watching = scope
            .observe_records(
                engine.clone(),
                predicate,
                vec![
                    FfiGroupRecord::Metadata,
                    FfiGroupRecord::Admins,
                    FfiGroupRecord::Members,
                ],
                None,
            )
            .expect("a two-host records observation opens");
        watching.cancel();
        engine.shutdown();
    }

    /// The empty record selection is refused at the boundary, not opened
    /// empty -- the same rule the direct-Rust door carries.
    #[test]
    fn an_empty_record_selection_is_a_typed_refusal_at_the_boundary() {
        let engine =
            NmpEngine::new(crate::facade::NmpEngineConfig::default(), None).expect("engine builds");
        let scope = FfiRelayScope::on(vec![host(1)]).expect("one host parses");
        match scope
            .group("photographers".to_string())
            .observe_records(engine.clone(), Vec::new())
        {
            Err(FfiError::GroupNoRecordSelected) => {}
            Err(other) => panic!("expected GroupNoRecordSelected, got {other:?}"),
            Ok(_) => panic!("an empty record selection must be refused, not opened"),
        }
        engine.shutdown();
    }

    /// #1245 at the boundary: a `read` selection naming the relay-signed
    /// records is refused with the kinds named, never answered with a
    /// permanently empty query.
    #[test]
    fn a_roster_read_through_the_content_door_is_refused_at_the_boundary() {
        let scope = FfiRelayScope::on(vec![host(1)]).expect("one host parses");
        let group = scope.group("photographers".to_string());
        match group.read(FfiFilter {
            kinds: Some(vec![39001, 39002]),
            ..FfiFilter::default()
        }) {
            Err(FfiError::GroupRecordsNotContextScoped { kinds }) => {
                assert_eq!(kinds, vec![39001, 39002]);
            }
            other => panic!("expected GroupRecordsNotContextScoped, got {other:?}"),
        }
    }

    /// The snapshot projection is lossless for everything an app renders --
    /// including the role a relay wrote, the absence of one it did not, and
    /// the hosts each entry is attributed to.
    #[test]
    fn snapshot_projection_is_lossless_for_what_an_app_renders() {
        use std::collections::{BTreeMap, BTreeSet};

        let with_role = nostr::Keys::generate().public_key();
        let without_role = nostr::Keys::generate().public_key();
        let relay = RelayUrl::parse(&host(1)).expect("a well-formed host");
        let metadata = GroupMetadata {
            name: Some("Photographers".to_string()),
            about: None,
            picture: None,
            tags: vec![vec!["parent".to_string(), "darkroom".to_string()]],
            as_of: nostr::Timestamp::from(1_700_000_000u64),
            event_id: nostr::EventId::all_zeros(),
            host: relay.clone(),
        };
        let snapshot = GroupSnapshot {
            id: "photographers".to_string(),
            metadata: Some(metadata.clone()),
            admins: vec![
                ListedSubject {
                    pubkey: with_role,
                    role: Some("moderator".to_string()),
                    hosts: BTreeSet::from([relay.clone()]),
                },
                ListedSubject {
                    pubkey: without_role,
                    role: None,
                    hosts: BTreeSet::from([relay.clone()]),
                },
            ],
            members: Vec::new(),
            availability: GroupAvailability::Ready,
            per_host: BTreeMap::from([(
                relay.clone(),
                nmp_nip29::HostRecords {
                    metadata: Some(metadata),
                    admins: None,
                    members: None,
                    availability: GroupAvailability::Ready,
                },
            )]),
            disagreements: BTreeSet::from([GroupRecord::Metadata]),
        };

        let projected = snapshot_to_ffi(&snapshot);
        assert_eq!(projected.id, "photographers");
        assert_eq!(
            projected.metadata.as_ref().and_then(|m| m.name.as_deref()),
            Some("Photographers")
        );
        assert_eq!(
            projected.metadata.as_ref().and_then(|m| m.about.clone()),
            None
        );
        assert_eq!(
            projected.metadata.as_ref().map(|m| m.tags.clone()),
            Some(vec![vec!["parent".to_string(), "darkroom".to_string()]]),
            "the raw rows cross the boundary so a native app needs no hand-parser"
        );
        assert_eq!(projected.admins[0].role.as_deref(), Some("moderator"));
        assert_eq!(
            projected.admins[1].role, None,
            "an absent role must cross the boundary as absent, never as a default"
        );
        assert_eq!(projected.admins[0].hosts, vec![host(1)]);
        assert_eq!(projected.availability, FfiGroupAvailability::Ready);
        assert_eq!(projected.per_host.len(), 1);
        assert_eq!(projected.per_host[0].host, host(1));
        assert_eq!(projected.disagreements, vec![FfiGroupRecord::Metadata]);
    }

    /// A literal `subjects` binding is validated as a pubkey, the same rule
    /// `FfiFilter.authors` carries -- never the unchecked arbitrary-tag rule.
    #[test]
    fn a_non_hex_literal_subject_is_a_typed_invalid_public_key() {
        match member_list_includes(FfiBinding::Literal {
            values: vec!["not-a-pubkey".to_string()],
        }) {
            Err(FfiError::InvalidPublicKey { got }) => assert_eq!(got, "not-a-pubkey"),
            other => panic!("expected InvalidPublicKey, got {other:?}"),
        }
    }

    /// `FfiGroup::publish`/every named operation reach the same one publish
    /// door, headless (no relay needs to be reachable for the write to be
    /// ACCEPTED at the engine's door -- delivery over real sockets is
    /// `crates/nmp/tests/group_publication_door.rs`'s job).
    #[test]
    fn every_named_group_operation_reaches_the_one_publish_door() {
        let engine =
            NmpEngine::new(crate::facade::NmpEngineConfig::default(), None).expect("engine builds");
        let author = nostr::Keys::generate().public_key().to_hex();
        let subject = nostr::Keys::generate().public_key().to_hex();
        let scope = FfiRelayScope::on(vec![host(1), host(2)]).expect("two hosts parse");
        let group = scope.group("photographers".to_string());

        let outcomes: Vec<(&str, Result<Arc<NmpReceiptStream>, FfiError>)> = vec![
            (
                "publish",
                group.publish(
                    engine.clone(),
                    author.clone(),
                    FfiEventBuilder {
                        kind: 9,
                        tags: vec![],
                        content: "first light".to_string(),
                        created_at: None,
                    },
                ),
            ),
            (
                "join_request",
                group.join_request(engine.clone(), author.clone(), Some("code".to_string())),
            ),
            (
                "leave_request",
                group.leave_request(engine.clone(), author.clone()),
            ),
            (
                "add_users",
                group.add_users(
                    engine.clone(),
                    author.clone(),
                    vec![FfiGroupUser {
                        pubkey: subject.clone(),
                        role: None,
                    }],
                ),
            ),
            (
                "remove_users",
                group.remove_users(engine.clone(), author.clone(), vec![subject.clone()]),
            ),
            (
                "edit_metadata",
                group.edit_metadata(
                    engine.clone(),
                    author.clone(),
                    FfiGroupMetadataEdit {
                        name: Some("Photographers".to_string()),
                        ..FfiGroupMetadataEdit::default()
                    },
                ),
            ),
            (
                "delete_event",
                group.delete_event(engine.clone(), author.clone(), "09".repeat(32)),
            ),
            (
                "create_group",
                group.create_group(engine.clone(), author.clone(), None),
            ),
            (
                "delete_group",
                group.delete_group(engine.clone(), author.clone()),
            ),
            (
                "create_invite",
                group.create_invite(engine.clone(), author.clone(), "code".to_string()),
            ),
        ];

        for (name, outcome) in outcomes {
            assert!(
                outcome.is_ok(),
                "{name} must reach the one publish door like every other group write"
            );
        }
    }

    /// #1281 across the boundary: a several-group write reaches the one
    /// publish door with the app naming neither a relay nor an `h` row, and
    /// comes back with the ordinary receipt stream.
    #[test]
    fn a_several_group_write_crosses_the_boundary_and_reaches_the_one_publish_door() {
        let engine =
            NmpEngine::new(crate::facade::NmpEngineConfig::default(), None).expect("engine builds");
        let scope = FfiRelayScope::on(vec![host(1), host(2)]).expect("two hosts parse");
        let rooms = scope
            .groups(vec!["darkroom".to_string(), "photographers".to_string()])
            .expect("a nonempty group set");
        let receipts = rooms
            .publish(
                engine,
                nostr::Keys::generate().public_key().to_hex(),
                FfiEventBuilder {
                    kind: 30315,
                    tags: vec![vec!["d".to_string(), "status".to_string()]],
                    content: String::new(),
                    created_at: None,
                },
            )
            .expect("the publish door accepts a several-group write");
        assert!(receipts.id() > 0, "a group write is a tracked write");
    }

    /// #1281's refusal at the boundary: naming no group forms no write
    /// context at all.
    #[test]
    fn a_write_context_over_no_group_is_never_formed_across_the_boundary() {
        let scope = FfiRelayScope::on(vec![host(1)]).expect("one host parses");
        match scope.groups(Vec::new()) {
            Err(FfiError::EmptyGroupSet) => {}
            other => panic!("expected EmptyGroupSet, got {other:?}"),
        }
    }

    /// #1282 across the boundary: the picture row and both marker rows reach
    /// the wire, so an app that wants a closed group no longer hand-writes
    /// `["closed"]` itself.
    #[test]
    fn the_metadata_edit_door_composes_the_picture_and_marker_rows() {
        let edit: GroupMetadataEdit = FfiGroupMetadataEdit {
            name: Some("Workspace".to_string()),
            picture: Some("https://cdn.example/w.png".to_string()),
            read_access: Some(FfiReadAccess::Public),
            join_access: Some(FfiJoinAccess::Closed),
            ..FfiGroupMetadataEdit::default()
        }
        .into();
        assert_eq!(
            nmp_nip29::edit_metadata(edit)
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect::<Vec<Vec<String>>>(),
            vec![
                vec!["name".to_string(), "Workspace".to_string()],
                vec![
                    "picture".to_string(),
                    "https://cdn.example/w.png".to_string()
                ],
                vec!["public".to_string()],
                vec!["closed".to_string()],
            ]
        );
    }

    /// #1301 across the boundary: an app declares a subgroup's parent on the
    /// kind:9007 CREATE, and a root group states its rootness by carrying no
    /// row at all.
    #[test]
    fn the_create_door_composes_the_parent_row_and_omits_it_for_a_root() {
        let rows = |parent: Option<String>| {
            nmp_nip29::create_group(parent.as_deref())
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect::<Vec<Vec<String>>>()
        };
        assert_eq!(
            rows(Some("darkroom".to_string())),
            vec![vec!["parent".to_string(), "darkroom".to_string()]]
        );
        assert!(
            rows(None).is_empty(),
            "a root group carries no parent row -- never an empty one"
        );
    }

    /// A caller-supplied `h` tag never reaches the door: the refusal is
    /// synchronous and typed, before any receipt stream exists.
    #[test]
    fn a_caller_supplied_context_never_reaches_the_door() {
        let engine =
            NmpEngine::new(crate::facade::NmpEngineConfig::default(), None).expect("engine builds");
        let author = nostr::Keys::generate().public_key().to_hex();
        let scope = FfiRelayScope::on(vec![host(1)]).expect("one host parses");
        let group = scope.group("photographers".to_string());

        let refused = group.publish(
            engine,
            author,
            FfiEventBuilder {
                kind: 9,
                tags: vec![vec!["h".to_string(), "photographers".to_string()]],
                content: String::new(),
                created_at: None,
            },
        );
        match refused {
            Err(FfiError::GroupCallerSuppliedContext) => {}
            Err(other) => panic!("expected GroupCallerSuppliedContext, got {other:?}"),
            Ok(_) => panic!("expected GroupCallerSuppliedContext, got Ok"),
        }
    }

    /// `validate_context` round trip: a correctly contextualized signed
    /// event validates, and a pre-signed event naming no group at all is a
    /// typed `GroupContextMissing`.
    #[test]
    fn validate_context_accepts_a_correctly_contextualized_event_and_refuses_a_bare_one() {
        let keys = nostr::Keys::generate();
        let scope = FfiRelayScope::on(vec![host(1)]).expect("one host parses");
        let group = scope.group("photographers".to_string());

        let contextualized = nostr::EventBuilder::new(nostr::Kind::from(9u16), "content")
            .tag(nostr::Tag::parse(["h", "photographers"]).unwrap())
            .sign_with_keys(&keys)
            .expect("a well-formed draft signs");
        assert!(group
            .validate_context(to_ffi_signed(&contextualized))
            .is_ok());

        let bare = nostr::EventBuilder::new(nostr::Kind::from(9u16), "content")
            .sign_with_keys(&keys)
            .expect("a well-formed draft signs");
        match group.validate_context(to_ffi_signed(&bare)) {
            Err(FfiError::GroupContextMissing { expected }) => {
                assert_eq!(expected, vec!["photographers".to_string()]);
            }
            other => panic!("expected GroupContextMissing, got {other:?}"),
        }
    }

    fn to_ffi_signed(event: &nostr::Event) -> FfiSignedEvent {
        FfiSignedEvent {
            id: event.id.to_hex(),
            pubkey: event.pubkey.to_hex(),
            created_at: event.created_at.as_secs(),
            kind: event.kind.as_u16(),
            tags: event.tags.iter().map(|tag| tag.clone().to_vec()).collect(),
            content: event.content.clone(),
            sig: event.sig.to_string(),
        }
    }

    /// `delete_event`'s `event_id` is parsed with the same typed
    /// `InvalidEventId` rule every other exact-hex event id input in this
    /// crate uses.
    #[test]
    fn delete_event_rejects_a_malformed_event_id() {
        let engine =
            NmpEngine::new(crate::facade::NmpEngineConfig::default(), None).expect("engine builds");
        let author = nostr::Keys::generate().public_key().to_hex();
        let scope = FfiRelayScope::on(vec![host(1)]).expect("one host parses");
        let group = scope.group("photographers".to_string());

        match group.delete_event(engine, author, "not-an-event-id".to_string()) {
            Err(FfiError::InvalidEventId { got }) => assert_eq!(got, "not-an-event-id"),
            Err(other) => panic!("expected InvalidEventId, got {other:?}"),
            Ok(_) => panic!("expected InvalidEventId, got Ok"),
        }
    }
}
