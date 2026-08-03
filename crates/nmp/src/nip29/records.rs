//! Watching NIP-29's relay-signed group records (#1233).
//!
//! `nmp-nip29` owns what one record SAYS. This module owns what several of
//! them, from several hosts, add up to over time: it observes one ordinary
//! [`LiveQuery`](crate::LiveQuery) through the one ordinary
//! [`Engine::observe_async`] door, folds the engine's [`RowDelta`]s into a row
//! map, and projects that map plus the frame's own
//! [`AcquisitionEvidence`] into a [`GroupSnapshot`] the app can render
//! directly. The app never sees a delta and never walks a `p` row.
//!
//! It is the same shape `nmp_nip02`'s follow observation already has -- one
//! query, an accumulator over `Added`/`Removed`, a per-frame projection, a
//! handle whose `Drop` withdraws the demand. It is NOT a second read door: no
//! socket, no subscription lifecycle, no retry and no cancellation semantics
//! live here that the engine does not already own. Delete
//! [`Engine::observe_async`] and nothing in this file can open anything.
//!
//! # What the aggregate across hosts may and may not claim
//!
//! NIP-29 authority is PER-RELAY. Two hosts serving the same group id are two
//! independent groups with the same name, so an aggregate over them has to
//! say something honest or say nothing.
//!
//! **The lists (39001/39002) UNION, with attribution.** Inclusion in an
//! observed list is evidence and absence from one is not evidence of the
//! opposite -- so a union asserts only true positives, and every entry
//! carries the hosts whose own record named it. Requiring agreement instead
//! would make the value perpetually absent: two relays' member sets are
//! essentially never identical.
//!
//! **The metadata (39000) does NOT union.** One host's whole record wins, the
//! one with the latest `created_at`, and it is delivered entire. Merging
//! field-wise -- A's newer `name` beside B's older `about` -- would synthesize
//! a record no relay ever signed and show it to a user as though one had.
//! Inclusion-is-evidence licenses a union over sets; there is no equivalent
//! argument for a scalar.
//!
//! **[`GroupSnapshot::differs`] answers whether the hosts disagree**, per
//! record, so an app can offer a dig-in affordance rather than pretending the
//! aggregate is the whole truth. [`GroupSnapshot::at`] is that dig-in: exactly
//! what one relay signed, nothing folded.
//!
//! # `as_of` is for display
//!
//! Every record carries the `created_at` its host signed it with. That is a
//! display fact about that relay's record and nothing else. Nothing here
//! compares it against a local clock or a local write time to adjudicate
//! whether a roster is "current": relays republish these records on unrelated
//! triggers, NIP-29 specifies no republication timing, and an adjudication
//! resting on that would rest on behaviour no relay guarantees.
//!
//! # Branches scale with HOSTS, not groups
//!
//! One branch per host, whatever the predicate matches. A hundred groups on
//! two hosts is two branches. [`LiveQuery::MAX_BRANCHES`](nmp_grammar::LiveQuery::MAX_BRANCHES)
//! is therefore a ceiling on HOSTS and never on groups. The pressure a large
//! watch list actually creates is on the `#d` value set inside one filter,
//! which a relay may refuse or silently truncate; see
//! [`GroupPredicate::AnyOf`](super::GroupPredicate::AnyOf).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use nostr::{Event, EventId, RelayUrl};

use nmp_nip29::{
    group_metadata_at, join_key_of, listed_record_at, GroupMetadata, GroupRecord, ListedRecord,
    ListedSubject,
};

use crate::engine::Engine;
use crate::error::EngineError;
use crate::subscription::AsyncSubscription;
use crate::{
    AcquisitionEvidence, ConcurrentNext, ObservationCancel, Row, RowDelta, ShortfallFact,
    SourceStatus,
};

use super::read::GroupReadError;

/// Why a group-records observation never opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupObserveError {
    /// The app selected no record at all. An empty selection would build a
    /// filter matching nothing and deliver a permanently empty snapshot,
    /// which is the exact indistinguishable-from-real-emptiness failure
    /// #1245 was about. Refused at the door instead.
    NoRecordSelected,
    /// The branches could not form one live query -- in practice a scope
    /// naming more HOSTS than one observation supports.
    Declaration(GroupReadError),
    /// The engine refused the observation.
    Engine(EngineError),
}

impl From<GroupReadError> for GroupObserveError {
    fn from(error: GroupReadError) -> Self {
        Self::Declaration(error)
    }
}

impl From<EngineError> for GroupObserveError {
    fn from(error: EngineError) -> Self {
        Self::Engine(error)
    }
}

impl std::fmt::Display for GroupObserveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRecordSelected => f.write_str(
                "a group-records observation must select at least one of the three relay-signed \
                 records",
            ),
            Self::Declaration(error) => write!(f, "{error}"),
            Self::Engine(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for GroupObserveError {}

/// Why a bounded wait produced no snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupWaitError {
    /// The deadline elapsed. The observation is untouched and still open --
    /// this is not a withdrawal, and awaiting again is correct.
    TimedOut,
    /// A second overlapping wait on the same observation. The stream is
    /// single-consumer, exactly like every other NMP subscription.
    Concurrent,
}

impl std::fmt::Display for GroupWaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut => f.write_str("no group-records snapshot arrived before the deadline"),
            Self::Concurrent => {
                f.write_str("a group-records observation is single-consumer; one wait at a time")
            }
        }
    }
}

impl std::error::Error for GroupWaitError {}

/// How much of what the app asked for has actually been established.
///
/// Hoisted onto the snapshot as the MINIMUM over every host in the scope, so
/// the ordinary case -- show a spinner while anything is still unproven --
/// never has to walk the per-host breakdown. A host that has reported nothing
/// yet counts as [`Self::Acquiring`]; it is not silently excluded from the
/// minimum, because "one of the two relays has not answered" is exactly the
/// state a spinner is for.
///
/// It says nothing about whether the records themselves are complete. A relay
/// that is `Ready` and published no member list has published no member list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GroupAvailability {
    /// A source failed hard (auth denied, transport error), or the plan has
    /// no honest acquisition path at all.
    SourceUnavailable,
    /// Still establishing. Nothing here is a claim of absence.
    Acquiring,
    /// Reconciled once, but the link is down now.
    CachedOnly,
    /// Every source in this host's plan has reconciled and is live.
    Ready,
}

/// Exactly what one host signed, folded with nothing.
///
/// This is the dig-in beside the aggregate. Each record is `Option` because a
/// relay genuinely may publish one, two, or none of the three -- 39001 and
/// 39002 are optional by NIP-29's own text -- and `None` here means "this
/// host has published none that we have seen", never "there are none".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRecords {
    /// This host's own kind:39000, entire.
    pub metadata: Option<GroupMetadata>,
    /// This host's own kind:39001, entire.
    pub admins: Option<ListedRecord>,
    /// This host's own kind:39002, entire.
    pub members: Option<ListedRecord>,
    /// This host's own acquisition state.
    pub availability: GroupAvailability,
}

/// One group, as the hosts in the scope currently describe it.
///
/// A complete self-contained value: it is never a patch on a previous one, so
/// a lost or redelivered delivery is benign and an app can render it
/// directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSnapshot {
    /// The `d` value the relay-signed records key themselves by.
    pub id: String,
    /// The whole winning host's kind:39000 -- latest `created_at` wins, ties
    /// broken by event id so two hosts stamped identically still produce a
    /// stable answer. Never merged field-wise. [`GroupMetadata::host`] says
    /// which relay signed the one being shown.
    pub metadata: Option<GroupMetadata>,
    /// The union of every host's kind:39001, each entry carrying the hosts
    /// that named it.
    pub admins: Vec<ListedSubject>,
    /// The union of every host's kind:39002, each entry carrying the hosts
    /// that named it.
    pub members: Vec<ListedSubject>,
    /// The minimum over every host in the scope.
    pub availability: GroupAvailability,
    /// Exactly what each host that answered signed, folded with nothing.
    pub per_host: BTreeMap<RelayUrl, HostRecords>,
    /// The records the answering hosts do not agree on. Computed while the
    /// union is built, at no extra pass.
    pub disagreements: BTreeSet<GroupRecord>,
}

impl GroupSnapshot {
    /// Exactly what `host` signed, or `None` if it has published none of the
    /// selected records for this group that we have seen.
    #[must_use]
    pub fn at(&self, host: &RelayUrl) -> Option<&HostRecords> {
        self.per_host.get(host)
    }

    /// Whether the hosts disagree about `record`.
    ///
    /// For a list: some host named a subject-and-role pair another host that
    /// also published that list did not. For the metadata: two hosts signed
    /// records that are not the same record. Either way an app can decide
    /// whether the aggregate is worth a dig-in affordance.
    #[must_use]
    pub fn differs(&self, record: GroupRecord) -> bool {
        self.disagreements.contains(&record)
    }
}

/// A live projection of NIP-29's relay-signed group records over one ordinary
/// NMP live query.
///
/// Dropping it withdraws the demand. NMP retains nothing keyed by group and
/// holds no handle on the app's behalf: the app owns this value's lifetime,
/// exactly as it owns every other observation's.
pub struct GroupObservation {
    subscription: AsyncSubscription,
    hosts: BTreeSet<RelayUrl>,
    seed_ids: BTreeSet<String>,
    accumulator: std::sync::Mutex<Accumulator>,
}

impl GroupObservation {
    /// Await the next snapshot set, or `None` once the demand is withdrawn.
    ///
    /// A scope-wide observation delivers one [`GroupSnapshot`] per group the
    /// predicate currently matches, in group-id order. A group-scoped one
    /// (via [`Group::observe`](super::Group::observe)) delivers exactly one,
    /// for the id it was narrowed to, from the first delivery onward --
    /// including before any record has arrived, so an app has an
    /// [`GroupAvailability::Acquiring`] snapshot to render immediately.
    pub async fn next(&self) -> Result<Option<Vec<GroupSnapshot>>, ConcurrentNext> {
        match self.subscription.next().await? {
            Some(frame) => {
                let mut accumulator = self
                    .accumulator
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                accumulator.apply(frame.deltas, &self.hosts);
                Ok(Some(project(
                    &self.hosts,
                    &self.seed_ids,
                    &accumulator,
                    &frame.evidence,
                )))
            }
            None => Ok(None),
        }
    }

    /// [`Self::next`] with a deadline. A timeout leaves the observation open.
    pub async fn next_within(
        &self,
        timeout: Duration,
    ) -> Result<Option<Vec<GroupSnapshot>>, GroupWaitError> {
        match tokio::time::timeout(timeout, self.next()).await {
            Ok(Ok(delivered)) => Ok(delivered),
            Ok(Err(_)) => Err(GroupWaitError::Concurrent),
            Err(_elapsed) => Err(GroupWaitError::TimedOut),
        }
    }

    /// The snapshots as the folded rows stand right now, without awaiting.
    ///
    /// Availability is deliberately absent from this reading -- it is a
    /// property of a delivered frame's evidence, and inventing one for a
    /// between-frames peek would be a claim no frame made. Every snapshot
    /// here reports [`GroupAvailability::Acquiring`] until a delivery says
    /// otherwise.
    #[must_use]
    pub fn latest(&self) -> Vec<GroupSnapshot> {
        let accumulator = self
            .accumulator
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        project(&self.hosts, &self.seed_ids, &accumulator, &[])
    }

    /// Withdraw the observation now (idempotent; `Drop` does the same).
    pub fn cancel(&self) {
        self.subscription.cancel();
    }

    #[must_use]
    pub fn cancel_handle(&self) -> ObservationCancel {
        self.subscription.cancel_handle()
    }
}

impl Drop for GroupObservation {
    fn drop(&mut self) {
        self.subscription.cancel();
    }
}

impl std::fmt::Debug for GroupObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupObservation")
            .field("hosts", &self.hosts)
            .finish_non_exhaustive()
    }
}

/// Open the observation. The only place in this module that touches the
/// engine, and it touches exactly the one ordinary observe door.
pub(super) fn observe(
    engine: &Engine,
    hosts: BTreeSet<RelayUrl>,
    seed_ids: BTreeSet<String>,
    branches: Vec<nmp_grammar::Demand>,
) -> Result<GroupObservation, GroupObserveError> {
    let query = super::read::one_live_query(branches)?;
    let subscription = engine.observe_async(query, None)?;
    Ok(GroupObservation {
        subscription,
        hosts,
        seed_ids,
        accumulator: std::sync::Mutex::new(Accumulator::default()),
    })
}

/// The folded row set. Keyed by event id exactly like `nmp_nip02`'s, because
/// that is what a `RowDelta` stream is keyed by.
#[derive(Default)]
struct Accumulator {
    rows: BTreeMap<EventId, (Event, BTreeSet<RelayUrl>)>,
}

impl Accumulator {
    fn apply(&mut self, deltas: Vec<RowDelta>, hosts: &BTreeSet<RelayUrl>) {
        for delta in deltas {
            match delta {
                RowDelta::Added(Row { event, sources }) => {
                    self.rows.insert(event.id, (event, &sources & hosts));
                }
                RowDelta::Removed(id) => {
                    self.rows.remove(&id);
                }
                // A second host serving a record the first already served is
                // real attribution news: the entry it produced must now name
                // both. `sources` only ever grows, so this is a widening.
                RowDelta::SourcesGrew { id, sources } => {
                    if let Some((_, attributed)) = self.rows.get_mut(&id) {
                        *attributed = &sources & hosts;
                    }
                }
            }
        }
    }
}

/// One host's acquisition state, read off the frame's own per-branch
/// evidence. Mirrors `nmp_nip02::service::availability`'s ladder, narrowed to
/// the sources that name THIS host.
fn availability_at(host: &RelayUrl, evidence: &[AcquisitionEvidence]) -> GroupAvailability {
    let hard_shortfall = evidence
        .iter()
        .flat_map(|branch| branch.shortfall.iter())
        .any(|fact| {
            matches!(
                fact,
                ShortfallFact::NoPlannedSource { .. } | ShortfallFact::LocalLimit { .. }
            )
        });
    if hard_shortfall {
        return GroupAvailability::SourceUnavailable;
    }

    let sources = || {
        evidence
            .iter()
            .flat_map(|branch| branch.sources.iter())
            .filter(|source| &source.relay == host)
    };
    if sources().any(|source| {
        matches!(
            source.status,
            SourceStatus::AuthDenied | SourceStatus::Error
        )
    }) {
        return GroupAvailability::SourceUnavailable;
    }
    if sources().next().is_none() || sources().any(|source| source.reconciled_through.is_none()) {
        return GroupAvailability::Acquiring;
    }
    if sources().any(|source| source.status == SourceStatus::Disconnected) {
        return GroupAvailability::CachedOnly;
    }
    if sources().all(|source| source.status == SourceStatus::Requesting) {
        GroupAvailability::Ready
    } else {
        GroupAvailability::Acquiring
    }
}

/// Everything the app is handed, derived from the folded rows and the frame's
/// evidence and from nothing else.
fn project(
    hosts: &BTreeSet<RelayUrl>,
    seed_ids: &BTreeSet<String>,
    accumulator: &Accumulator,
    evidence: &[AcquisitionEvidence],
) -> Vec<GroupSnapshot> {
    let per_host_availability: BTreeMap<&RelayUrl, GroupAvailability> = hosts
        .iter()
        .map(|host| (host, availability_at(host, evidence)))
        .collect();
    let availability = per_host_availability
        .values()
        .copied()
        .min()
        .unwrap_or(GroupAvailability::Acquiring);

    // id -> host -> record -> the newest event that host signed.
    let mut folded: BTreeMap<String, BTreeMap<&RelayUrl, HostFold>> = seed_ids
        .iter()
        .map(|id| (id.clone(), BTreeMap::new()))
        .collect();

    for (event, attributed) in accumulator.rows.values() {
        let Some(record) = GroupRecord::of_kind(event.kind.as_u16()) else {
            continue;
        };
        let Some(id) = join_key_of(event) else {
            continue;
        };
        for host in attributed {
            let Some(host) = hosts.get(host) else {
                continue;
            };
            folded
                .entry(id.clone())
                .or_default()
                .entry(host)
                .or_default()
                .offer(record, event, host);
        }
    }

    folded
        .into_iter()
        .map(|(id, hosts_fold)| {
            let per_host: BTreeMap<RelayUrl, HostRecords> = hosts_fold
                .iter()
                .map(|(host, fold)| {
                    (
                        (*host).clone(),
                        HostRecords {
                            metadata: fold.metadata.clone(),
                            admins: fold.admins.clone(),
                            members: fold.members.clone(),
                            availability: per_host_availability
                                .get(host)
                                .copied()
                                .unwrap_or(GroupAvailability::Acquiring),
                        },
                    )
                })
                .collect();

            let mut disagreements = BTreeSet::new();

            // Metadata: event-wise latest wins. NEVER field-wise.
            let metadata = per_host
                .values()
                .filter_map(|records| records.metadata.as_ref())
                .max_by(|left, right| {
                    left.as_of
                        .cmp(&right.as_of)
                        .then_with(|| left.event_id.cmp(&right.event_id))
                })
                .cloned();
            // What a host SAID, without the per-host bookkeeping (which host
            // signed it, which event it was) that necessarily differs.
            let said = |record: &GroupMetadata| {
                (
                    record.name.clone(),
                    record.about.clone(),
                    record.picture.clone(),
                    record.tags.clone(),
                )
            };
            if let Some(winner) = metadata.as_ref() {
                if per_host
                    .values()
                    .filter_map(|records| records.metadata.as_ref())
                    .any(|other| said(other) != said(winner))
                {
                    disagreements.insert(GroupRecord::Metadata);
                }
            }

            let admins = union(
                &per_host,
                GroupRecord::Admins,
                &mut disagreements,
                |records| records.admins.as_ref(),
            );
            let members = union(
                &per_host,
                GroupRecord::Members,
                &mut disagreements,
                |records| records.members.as_ref(),
            );

            GroupSnapshot {
                id,
                metadata,
                admins,
                members,
                availability,
                per_host,
                disagreements,
            }
        })
        .collect()
}

/// Union one list across the hosts that published it, merging attribution.
///
/// Seeded from EVERY host that published the record, never from the first one
/// that answered: a subject listed solely by the second host must still
/// appear. Disagreement is whatever falls out -- an entry not named by every
/// host that published this list at all.
fn union(
    per_host: &BTreeMap<RelayUrl, HostRecords>,
    record: GroupRecord,
    disagreements: &mut BTreeSet<GroupRecord>,
    select: impl Fn(&HostRecords) -> Option<&ListedRecord>,
) -> Vec<ListedSubject> {
    let publishing: Vec<&ListedRecord> = per_host.values().filter_map(select).collect();
    let mut merged: BTreeMap<(nostr::PublicKey, Option<String>), BTreeSet<RelayUrl>> =
        BTreeMap::new();
    for listed in &publishing {
        for subject in &listed.subjects {
            merged
                .entry((subject.pubkey, subject.role.clone()))
                .or_default()
                .extend(subject.hosts.iter().cloned());
        }
    }
    if merged.values().any(|hosts| hosts.len() != publishing.len()) {
        disagreements.insert(record);
    }
    merged
        .into_iter()
        .map(|((pubkey, role), hosts)| ListedSubject {
            pubkey,
            role,
            hosts,
        })
        .collect()
}

/// One host's newest record of each kind, while folding.
#[derive(Default)]
struct HostFold {
    metadata: Option<GroupMetadata>,
    admins: Option<ListedRecord>,
    members: Option<ListedRecord>,
}

impl HostFold {
    fn offer(&mut self, record: GroupRecord, event: &Event, host: &RelayUrl) {
        match record {
            GroupRecord::Metadata => {
                let candidate = group_metadata_at(host, event);
                if self.metadata.as_ref().is_none_or(|held| {
                    newer(
                        candidate.as_of,
                        candidate.event_id,
                        held.as_of,
                        held.event_id,
                    )
                }) {
                    self.metadata = Some(candidate);
                }
            }
            GroupRecord::Admins => offer_list(&mut self.admins, listed_record_at(host, event)),
            GroupRecord::Members => offer_list(&mut self.members, listed_record_at(host, event)),
        }
    }
}

fn offer_list(slot: &mut Option<ListedRecord>, candidate: ListedRecord) {
    if slot.as_ref().is_none_or(|held| {
        newer(
            candidate.as_of,
            candidate.event_id,
            held.as_of,
            held.event_id,
        )
    }) {
        *slot = Some(candidate);
    }
}

/// Later `created_at` wins; an exact tie is broken by event id so the same row
/// set always folds to the same answer.
fn newer(
    candidate_as_of: nostr::Timestamp,
    candidate_id: EventId,
    held_as_of: nostr::Timestamp,
    held_id: EventId,
) -> bool {
    (candidate_as_of, candidate_id) > (held_as_of, held_id)
}
