//! Native projection of `nmp::nip29` -- the app-facing NIP-29 door (#1033,
//! Lane A of the #1033 FFI projection).
//!
//! Two objects, same narrowing the direct-Rust door uses:
//!
//! ```text
//! let scope = FfiRelayScope.on([hostA, hostB])   // which relays
//! let group = scope.group("photographers")       // narrowed to one group
//! ```
//!
//! [`FfiRelayScope`] wraps [`nmp::nip29::RelayScope`] and [`FfiGroup`] wraps
//! [`nmp::nip29::Group`] -- both opaque UniFFI objects (the same idiom
//! [`crate::blossom::FfiBlossomAuthorization`] uses for a proven Rust value
//! carried across the boundary), never a second mirrored copy of NIP-29's
//! own vocabulary. Neither type exposes its retained hosts or group id back
//! out: exactly like the Rust door, there is no spelling for composing an
//! event under one group and routing it as though it came from another.
//!
//! [`FfiGroupPredicate`] wraps [`nmp::nip29::GroupPredicate`] the same way.
//! It stays opaque for the same reason `FfiBinding::Derived`/`FfiBinding::
//! SetOp` are UniFFI objects rather than records (see `types.rs`'s own doc):
//! a caller composes it with [`member_list_includes`]/[`admin_list_includes`]
//! and [`FfiGroupPredicate::union`]/[`intersect`]/[`minus`], then hands it to
//! [`FfiRelayScope::observe_records`] -- it is never inspected.
//!
//! Deliberately absent, same as before #1033: a fixed group-content kind
//! catalog and a kind:9 composer. NIP-29 owns neither; C7 and client
//! notification policy remain independently optional (#838). Also absent:
//! any second projection of a NIP-51 Simple-groups entry -- [`nip51`](crate::nip51)
//! keeps that one shape, and a caller wanting to browse a group hands its
//! `host_relay`/`group_id` fields to [`FfiRelayScope::on`]/[`FfiRelayScope::group`]
//! itself.

use std::sync::Arc;

use nmp::nip29::{
    self, Group, GroupAvailability, GroupMetadata, GroupObservation, GroupPredicate, GroupRecord,
    GroupSnapshot, ListedRecord, ListedSubject, RelayScope,
};
use nostr::RelayUrl;

use crate::convert::{
    event_builder_from_ffi, filter_from_ffi, live_query_to_ffi, parse_event_id, parse_pubkey,
    signed_event_from_ffi, subjects_binding_from_ffi, write_status_to_ffi, FfiError,
    WriteStatusRef,
};
use crate::facade::NmpEngine;
use crate::types::{
    FfiBinding, FfiEventBuilder, FfiFilter, FfiLiveQuery, FfiSignedEvent, FfiWriteStatus,
};

fn parse_host(host: String) -> Result<RelayUrl, FfiError> {
    RelayUrl::parse(&host).map_err(|_| FfiError::InvalidRelayUrl { got: host })
}

/// The relays a group lives on -- named once, retained privately inside the
/// opaque handle, and never asked for again (`nmp::nip29::RelayScope`
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
    /// Name the relays a NIP-29 group lives on (`nmp::nip29::on` mirror).
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
    /// (`nmp::nip29::RelayScope::group` mirror). Contacts nothing.
    pub fn group(&self, group_id: String) -> Arc<FfiGroup> {
        Arc::new(FfiGroup {
            inner: self.inner.group(group_id),
        })
    }

    /// Watch the relay-signed records of every group matching `predicate`
    /// (`nmp::nip29::RelayScope::observe` mirror). One complete branch per
    /// host, folded into ONE ordinary engine subscription; each delivery is
    /// the complete set of [`FfiGroupSnapshot`]s for the groups currently
    /// matching. The app never sees a row delta and never walks a `p` row.
    pub fn observe_records(
        &self,
        engine: Arc<NmpEngine>,
        predicate: Arc<FfiGroupPredicate>,
        records: Vec<FfiGroupRecord>,
    ) -> Result<Arc<NmpGroupRecordsStream>, FfiError> {
        let observation = self.inner.observe(
            &engine.engine,
            predicate.inner.clone(),
            records.into_iter().map(GroupRecord::from),
        )?;
        Ok(NmpGroupRecordsStream::new(observation))
    }
}

/// One NIP-29 group, on the relays its scope named (`nmp::nip29::Group`
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
    /// (`nmp::nip29::Group::read` mirror). A selection that already
    /// constrains `#h` is refused with
    /// [`FfiError::GroupCallerSuppliedContextConstraint`] -- the retained
    /// group id is the sole semantic source of that row. Hand the result to
    /// `NmpEngine::observe_query`.
    pub fn read(&self, selection: FfiFilter) -> Result<FfiLiveQuery, FfiError> {
        let selection = filter_from_ffi(selection)?;
        let query = self.inner.read(selection)?;
        Ok(live_query_to_ffi(query))
    }

    /// Watch THIS group's own relay-signed records
    /// (`nmp::nip29::Group::observe` mirror). Each delivery carries exactly
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
    /// building a write out of it (`nmp::nip29::Group::validate_context`
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

    /// Publish any unsigned draft into the group, as `author`
    /// (`nmp::nip29::Group::publish` mirror). `author` is an exact decoded
    /// pubkey, never the active-account selector: a semantic group write
    /// freezes who is writing at composition time (#878).
    pub fn publish(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        builder: FfiEventBuilder,
    ) -> Result<Arc<NmpGroupReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let builder = event_builder_from_ffi(builder)?;
        let receipts = self.inner.publish(&engine.engine, author, builder)?;
        Ok(NmpGroupReceiptStream::new(receipts))
    }

    /// Publish an ALREADY-SIGNED event into the group
    /// (`nmp::nip29::Group::publish_signed` mirror). The `h` it already
    /// carries is validated, never appended or repaired -- see
    /// [`Self::validate_context`]'s doc for the exact refusals.
    pub fn publish_signed(
        &self,
        engine: Arc<NmpEngine>,
        event: FfiSignedEvent,
    ) -> Result<Arc<NmpGroupReceiptStream>, FfiError> {
        let event = signed_event_from_ffi(
            event.id,
            event.pubkey,
            event.created_at,
            event.kind,
            event.tags,
            event.content,
            event.sig,
        )?;
        let receipts = self.inner.publish_signed(&engine.engine, event)?;
        Ok(NmpGroupReceiptStream::new(receipts))
    }

    /// kind:9021 -- ask to join. Publishable with no subscription at all.
    pub fn join_request(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        invite_code: Option<String>,
    ) -> Result<Arc<NmpGroupReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let receipts = self
            .inner
            .join_request(&engine.engine, author, invite_code.as_deref())?;
        Ok(NmpGroupReceiptStream::new(receipts))
    }

    /// kind:9022 -- leave.
    pub fn leave_request(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
    ) -> Result<Arc<NmpGroupReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let receipts = self.inner.leave_request(&engine.engine, author)?;
        Ok(NmpGroupReceiptStream::new(receipts))
    }

    /// kind:9000 -- add a member, optionally with a role.
    pub fn add_user(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        pubkey: String,
        role: Option<String>,
    ) -> Result<Arc<NmpGroupReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let pubkey = parse_pubkey(&pubkey)?;
        let receipts = self
            .inner
            .add_user(&engine.engine, author, pubkey, role.as_deref())?;
        Ok(NmpGroupReceiptStream::new(receipts))
    }

    /// kind:9001 -- remove a member.
    pub fn remove_user(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        pubkey: String,
    ) -> Result<Arc<NmpGroupReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let pubkey = parse_pubkey(&pubkey)?;
        let receipts = self.inner.remove_user(&engine.engine, author, pubkey)?;
        Ok(NmpGroupReceiptStream::new(receipts))
    }

    /// kind:9002 -- set the group's display fields. An omitted field emits
    /// no tag at all, so it is left untouched rather than cleared.
    pub fn edit_metadata(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        name: Option<String>,
        about: Option<String>,
    ) -> Result<Arc<NmpGroupReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let receipts =
            self.inner
                .edit_metadata(&engine.engine, author, name.as_deref(), about.as_deref())?;
        Ok(NmpGroupReceiptStream::new(receipts))
    }

    /// kind:9005 -- delete one group-hosted event.
    pub fn delete_event(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        event_id: String,
    ) -> Result<Arc<NmpGroupReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let event_id = parse_event_id(&event_id)?;
        let receipts = self.inner.delete_event(&engine.engine, author, event_id)?;
        Ok(NmpGroupReceiptStream::new(receipts))
    }

    /// kind:9007 -- create the group at its hosts.
    pub fn create_group(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
    ) -> Result<Arc<NmpGroupReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let receipts = self.inner.create_group(&engine.engine, author)?;
        Ok(NmpGroupReceiptStream::new(receipts))
    }

    /// kind:9008 -- delete the group from its hosts.
    pub fn delete_group(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
    ) -> Result<Arc<NmpGroupReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let receipts = self.inner.delete_group(&engine.engine, author)?;
        Ok(NmpGroupReceiptStream::new(receipts))
    }

    /// kind:9009 -- mint an invite code redeemable by
    /// [`Self::join_request`].
    pub fn create_invite(
        &self,
        engine: Arc<NmpEngine>,
        author: String,
        code: String,
    ) -> Result<Arc<NmpGroupReceiptStream>, FfiError> {
        let author = parse_pubkey(&author)?;
        let receipts = self.inner.create_invite(&engine.engine, author, &code)?;
        Ok(NmpGroupReceiptStream::new(receipts))
    }
}

/// A composable NIP-29 discovery predicate (`nmp::nip29::GroupPredicate`
/// mirror). Opaque by design -- see this module's own doc for why -- built
/// with [`member_list_includes`]/[`admin_list_includes`] and composed with
/// [`Self::union`]/[`Self::intersect`]/[`Self::minus`], then handed to
/// [`FfiRelayScope::observe_records`].
#[derive(Debug, uniffi::Object)]
pub struct FfiGroupPredicate {
    inner: GroupPredicate,
}

#[uniffi::export]
impl FfiGroupPredicate {
    /// Groups matching this predicate OR any of `others`.
    pub fn union(&self, others: Vec<Arc<FfiGroupPredicate>>) -> Arc<Self> {
        let inner = self
            .inner
            .clone()
            .union(others.iter().map(|other| other.inner.clone()));
        Arc::new(Self { inner })
    }

    /// Groups matching this predicate AND all of `others`.
    pub fn intersect(&self, others: Vec<Arc<FfiGroupPredicate>>) -> Arc<Self> {
        let inner = self
            .inner
            .clone()
            .intersect(others.iter().map(|other| other.inner.clone()));
        Arc::new(Self { inner })
    }

    /// Groups matching this predicate and none of `others`.
    pub fn minus(&self, others: Vec<Arc<FfiGroupPredicate>>) -> Arc<Self> {
        let inner = self
            .inner
            .clone()
            .minus(others.iter().map(|other| other.inner.clone()));
        Arc::new(Self { inner })
    }
}

/// Groups whose observed kind:39002 member-list evidence names `subjects`
/// (`nmp::nip29::member_list_includes` mirror). Inclusion is evidence,
/// never exact state -- absence is not evidence of non-membership.
#[uniffi::export]
pub fn member_list_includes(subjects: FfiBinding) -> Result<Arc<FfiGroupPredicate>, FfiError> {
    let subjects = subjects_binding_from_ffi(subjects)?;
    Ok(Arc::new(FfiGroupPredicate {
        inner: nip29::member_list_includes(subjects),
    }))
}

/// Groups whose observed kind:39001 admin-list evidence names `subjects`
/// (`nmp::nip29::admin_list_includes` mirror). Evidence-scoped exactly like
/// [`member_list_includes`].
#[uniffi::export]
pub fn admin_list_includes(subjects: FfiBinding) -> Result<Arc<FfiGroupPredicate>, FfiError> {
    let subjects = subjects_binding_from_ffi(subjects)?;
    Ok(Arc::new(FfiGroupPredicate {
        inner: nip29::admin_list_includes(subjects),
    }))
}

/// Which of NIP-29's three relay-signed group records an app is asking for
/// (`nmp::nip29::GroupRecord` mirror).
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
/// (`nmp::nip29::GroupAvailability` mirror). Says nothing about whether the
/// records themselves are complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiGroupAvailability {
    SourceUnavailable,
    Acquiring,
    CachedOnly,
    Ready,
}

/// One subject a relay-signed list names, and the hosts that named it
/// (`nmp::nip29::ListedSubject` mirror). `role` is absent when the relay
/// wrote none -- never defaulted.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiListedSubject {
    pub pubkey: String,
    pub role: Option<String>,
    pub hosts: Vec<String>,
}

/// One relay-signed list record (`nmp::nip29::ListedRecord` mirror).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiListedRecord {
    pub subjects: Vec<FfiListedSubject>,
    /// The record's own `created_at`. A DISPLAY fact -- never compared
    /// against a local clock to adjudicate anything.
    pub as_of: u64,
    pub event_id: String,
    pub host: String,
}

/// One relay-signed kind:39000 record (`nmp::nip29::GroupMetadata` mirror).
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
/// (`nmp::nip29::HostRecords` mirror).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiHostRecords {
    pub host: String,
    pub metadata: Option<FfiGroupMetadata>,
    pub admins: Option<FfiListedRecord>,
    pub members: Option<FfiListedRecord>,
    pub availability: FfiGroupAvailability,
}

/// One group, as the hosts in the scope currently describe it
/// (`nmp::nip29::GroupSnapshot` mirror). A complete self-contained value,
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

/// Pull-based group-records observation handle (`nmp::nip29::GroupObservation`
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
        as_of: record.as_of.as_u64(),
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
        as_of: metadata.as_of.as_u64(),
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

/// Exactly these group ids, whatever any list says about them
/// (`nmp::nip29::any_of` mirror). The leaf an app uses when it already knows
/// which rooms it is showing and is not asking a relational question.
#[uniffi::export]
pub fn any_of(ids: Vec<String>) -> Arc<FfiGroupPredicate> {
    Arc::new(FfiGroupPredicate {
        inner: nip29::any_of(ids),
    })
}

/// Pull-based receipt stream for a NIP-29 group write (#1033). Unlike
/// [`crate::facade::NmpReceiptStream`] this stream carries NO receipt id:
/// every `FfiGroup` write reaches the engine's UNTRACKED `Engine::publish`
/// door (never `publish_tracked`), because the store-issued receipt-id
/// namespace is a `publish`-door concern the group scope has no reason to
/// surface -- `nmp::nip29::Group::through_the_one_door`'s own doc names the
/// same door. Same ordered `WriteStatus` delivery and cancel/Drop discipline
/// as every other pull stream in this crate.
#[derive(uniffi::Object)]
pub struct NmpGroupReceiptStream {
    inner: nmp::AsyncFifoReceiver<nmp::WriteStatus>,
}

impl NmpGroupReceiptStream {
    fn new(receipts: nmp::FifoReceiver<nmp::WriteStatus>) -> Arc<Self> {
        Arc::new(Self {
            inner: receipts.into_async(),
        })
    }
}

#[uniffi::export]
impl NmpGroupReceiptStream {
    /// Await the next `WriteStatus`, or `None` once the write has fully
    /// resolved or the engine has shut down. [`FfiError::ConcurrentNext`] on
    /// an overlapping call.
    pub async fn next(&self) -> Result<Option<FfiWriteStatus>, FfiError> {
        match self.inner.next().await {
            Ok(Some(status)) => Ok(Some(write_status_to_ffi(WriteStatusRef(&status)))),
            Ok(None) => Ok(None),
            Err(nmp::FifoNextError::ConcurrentNext) => Err(FfiError::ConcurrentNext),
            Err(nmp::FifoNextError::Lagged) => Err(FfiError::FactStreamLagged { receipt_id: None }),
        }
    }

    /// Withdraw this stream now, rather than waiting for `Drop`. Safe to
    /// call more than once; safe to never call at all.
    pub fn cancel(&self) {
        self.inner.close();
    }
}

impl Drop for NmpGroupReceiptStream {
    fn drop(&mut self) {
        self.inner.close();
    }
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
    /// FFI mirror of `nmp::nip29::Group::read`'s own falsifier.
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
            NmpEngine::new(crate::facade::NmpEngineConfig::default()).expect("engine builds");
        let scope = FfiRelayScope::on(vec![host(1), host(2)]).expect("two hosts parse");
        let member = member_list_includes(FfiBinding::Reactive {
            field: FfiIdentityField::ActivePubkey,
        })
        .expect("a reactive subjects binding needs no hex validation");
        let admin = admin_list_includes(FfiBinding::Reactive {
            field: FfiIdentityField::ActivePubkey,
        })
        .expect("a reactive subjects binding needs no hex validation");
        let predicate = member.union(vec![admin, any_of(vec!["photographers".to_string()])]);

        let watching = scope
            .observe_records(
                engine.clone(),
                predicate,
                vec![
                    FfiGroupRecord::Metadata,
                    FfiGroupRecord::Admins,
                    FfiGroupRecord::Members,
                ],
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
            NmpEngine::new(crate::facade::NmpEngineConfig::default()).expect("engine builds");
        let scope = FfiRelayScope::on(vec![host(1)]).expect("one host parses");
        match scope.group("photographers".to_string()).observe_records(
            engine.clone(),
            Vec::new(),
        ) {
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
                nmp::nip29::HostRecords {
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
        assert_eq!(projected.metadata.as_ref().and_then(|m| m.about.clone()), None);
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
            NmpEngine::new(crate::facade::NmpEngineConfig::default()).expect("engine builds");
        let author = nostr::Keys::generate().public_key().to_hex();
        let subject = nostr::Keys::generate().public_key().to_hex();
        let scope = FfiRelayScope::on(vec![host(1), host(2)]).expect("two hosts parse");
        let group = scope.group("photographers".to_string());

        let outcomes: Vec<(&str, Result<Arc<NmpGroupReceiptStream>, FfiError>)> = vec![
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
                "add_user",
                group.add_user(engine.clone(), author.clone(), subject.clone(), None),
            ),
            (
                "remove_user",
                group.remove_user(engine.clone(), author.clone(), subject.clone()),
            ),
            (
                "edit_metadata",
                group.edit_metadata(
                    engine.clone(),
                    author.clone(),
                    Some("Photographers".to_string()),
                    None,
                ),
            ),
            (
                "delete_event",
                group.delete_event(engine.clone(), author.clone(), "09".repeat(32)),
            ),
            (
                "create_group",
                group.create_group(engine.clone(), author.clone()),
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

    /// A caller-supplied `h` tag never reaches the door: the refusal is
    /// synchronous and typed, before any receipt stream exists.
    #[test]
    fn a_caller_supplied_context_never_reaches_the_door() {
        let engine =
            NmpEngine::new(crate::facade::NmpEngineConfig::default()).expect("engine builds");
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
                assert_eq!(expected, "photographers");
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
            NmpEngine::new(crate::facade::NmpEngineConfig::default()).expect("engine builds");
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
