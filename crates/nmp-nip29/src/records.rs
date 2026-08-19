//! What the three relay-signed NIP-29 records actually SAY (#1233).
//!
//! `discovery.rs` owns which kinds describe a group and how they join on `d`.
//! This module owns the other half of the same schema: what one such record
//! carries once you read it. [`group_metadata_at`] turns a kind:39000 event
//! into a [`GroupMetadata`]; [`listed_record_at`] turns a kind:39001 or
//! kind:39002 event into a [`ListedRecord`] of [`ListedSubject`]s.
//!
//! # Why the projection lives beside the schema
//!
//! Before this existed the crate exposed the three kinds as PREDICATES only
//! -- `member_list_includes_at`, `admin_list_includes_at` -- each answering
//! "does this list name X" and none answering "who does this list name". So
//! both real consumer applications walked the `p` rows themselves, four
//! times, and the four readings disagreed: one dropped the role field another
//! kept, and one recorded a role-less admin as a member. A crate that owns a
//! schema and does not own the only correct way to read it produces exactly
//! that.
//!
//! # Evidence, never exact state
//!
//! Reading a list does not change what a list IS. kind:39002 remains an
//! optional, possibly partial member-list snapshot and kind:39001 an optional
//! informative admin list; inclusion is evidence and ABSENCE IS NOT EVIDENCE
//! of the opposite. What comes back here is what one relay published, spelled
//! as such: a [`ListedRecord`] signed by one host at one moment, never "the
//! members".
//!
//! # Roles
//!
//! kind:39001 spells its rows `["p", pubkey, role]` and kind:39002 spells
//! them `["p", pubkey]`, so [`ListedSubject::role`] is an `Option<String>`
//! that is `None` when the relay wrote no role. It is NEVER defaulted to
//! `"member"`: doing so silently records a role-less admin as a member, which
//! is a real shipped defect in one of the hand-rolled readers this module
//! replaces. Reading is kind-blind for the same reason -- a role position the
//! relay filled is reported, and one it left empty is reported as absent, on
//! whichever record it appears.
//!
//! # Typed fields AND the raw rows
//!
//! [`GroupMetadata`] carries the three value rows NIP-29 itself names
//! (`name`, `about`, `picture`) as typed fields AND the record's complete row
//! list verbatim. Both consumer applications read a `parent` row that NIP-29
//! core does not define; typed-fields-only would force them to keep a
//! hand-parser alive for it, which is the very thing this module exists to
//! delete. The same shape `FfiRelayInformationDocument` already uses for a
//! document whose useful fields are known and whose full content is not.

use std::collections::BTreeSet;

use nmp_grammar::{Binding, Demand, Filter};
use nostr::{Event, EventId, PublicKey, RelayUrl, Timestamp};

use crate::discovery::{
    join_key, explicit_at, subject, GROUP_ADMINS_KIND, GROUP_MEMBERS_KIND, GROUP_METADATA_KIND,
};

/// Which of NIP-29's three relay-signed group records an app is asking for.
///
/// The selector exists because asking for all three is frequently wrong: a
/// directory screen wants metadata alone, a moderation control wants the
/// admin list alone, and paying for a member list nobody renders is a
/// per-relay cost with no reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GroupRecord {
    /// kind:39000 -- the group's own metadata.
    Metadata,
    /// kind:39001 -- the optional, informative admin list.
    Admins,
    /// kind:39002 -- the optional, possibly partial member list.
    Members,
}

impl GroupRecord {
    /// Every record NIP-29 defines, in a stable order.
    pub const ALL: [Self; 3] = [Self::Metadata, Self::Admins, Self::Members];

    /// The kind NIP-29 publishes this record as.
    #[must_use]
    pub fn record_kind(self) -> u16 {
        match self {
            Self::Metadata => GROUP_METADATA_KIND,
            Self::Admins => GROUP_ADMINS_KIND,
            Self::Members => GROUP_MEMBERS_KIND,
        }
    }

    /// Which record a kind is, or `None` for a kind that is not one of
    /// NIP-29's three. Total: it classifies rather than asserting, so a
    /// caller folding a mixed row set is never forced to panic on something
    /// it did not select.
    #[must_use]
    pub fn of_kind(kind: u16) -> Option<Self> {
        match kind {
            GROUP_METADATA_KIND => Some(Self::Metadata),
            GROUP_ADMINS_KIND => Some(Self::Admins),
            GROUP_MEMBERS_KIND => Some(Self::Members),
            _ => None,
        }
    }
}

/// One subject a relay-signed list names, and the hosts that named it.
///
/// `hosts` is one element for a subject read off a single record and grows
/// only when the same `(pubkey, role)` pair is unioned across hosts. It is
/// the attribution that makes a cross-host union honest: a union asserts only
/// true positives, and `hosts` says exactly which relay supported each one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ListedSubject {
    /// The subject itself, decoded. Never a bech32 string.
    pub pubkey: PublicKey,
    /// The role row the relay wrote beside the subject, verbatim, or `None`
    /// when it wrote none. Never defaulted.
    pub role: Option<String>,
    /// The hosts whose own record named this exact subject-and-role pair.
    pub hosts: BTreeSet<RelayUrl>,
}

/// One relay-signed list record: whom it names, when that host signed it, and
/// which event it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedRecord {
    /// The subjects in the relay's own row order, deduplicated on the exact
    /// `(pubkey, role)` pair the relay wrote.
    pub subjects: Vec<ListedSubject>,
    /// The record's own `created_at`. A DISPLAY fact about this relay's
    /// record: it is never compared against a local clock or a local write
    /// time to adjudicate anything.
    pub as_of: Timestamp,
    /// The exact event this reading came from.
    pub event_id: EventId,
    /// The host that signed it.
    pub host: RelayUrl,
}

/// One relay-signed kind:39000 record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMetadata {
    /// The `name` row, if the record carries one.
    pub name: Option<String>,
    /// The `about` row, if the record carries one.
    pub about: Option<String>,
    /// The `picture` row, if the record carries one.
    pub picture: Option<String>,
    /// Every row of the record, verbatim and in the relay's own order --
    /// including the ones NIP-29 core does not define and the marker rows it
    /// spells without a value.
    pub tags: Vec<Vec<String>>,
    /// The record's own `created_at`. A DISPLAY fact, exactly as on
    /// [`ListedRecord::as_of`].
    pub as_of: Timestamp,
    /// The exact event this reading came from.
    pub event_id: EventId,
    /// The host that signed it.
    pub host: RelayUrl,
}

/// The `d` value a relay-signed group record keys itself by, or `None` for an
/// event carrying no `d` row.
#[must_use]
pub fn join_key_of(event: &Event) -> Option<String> {
    first_value(event, &join_key().to_string())
}

/// Read one kind:39000 record signed by `host`.
#[must_use]
pub fn group_metadata_at(host: &RelayUrl, event: &Event) -> GroupMetadata {
    GroupMetadata {
        name: first_value(event, "name"),
        about: first_value(event, "about"),
        picture: first_value(event, "picture"),
        tags: event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        as_of: event.created_at,
        event_id: event.id,
        host: host.clone(),
    }
}

/// Read one kind:39001 or kind:39002 record signed by `host`.
///
/// Blind to which of the two records this is: the role position is reported when the relay filled
/// it and reported absent when it did not, on whichever of the two records it
/// appears. A row whose subject does not decode as a public key is DROPPED
/// rather than guessed at -- NMP has no honest reading of it, and inventing
/// one would be the same defect as defaulting a role.
#[must_use]
pub fn listed_record_at(host: &RelayUrl, event: &Event) -> ListedRecord {
    let name = subject().to_string();
    let mut subjects: Vec<ListedSubject> = Vec::new();
    for tag in event.tags.iter() {
        let row = tag.as_slice();
        if row.first().map(String::as_str) != Some(name.as_str()) {
            continue;
        }
        let Some(pubkey) = row.get(1).and_then(|value| PublicKey::parse(value).ok()) else {
            continue;
        };
        let role = row
            .get(2)
            .map(String::to_string)
            .filter(|role| !role.is_empty());
        let listed = ListedSubject {
            pubkey,
            role,
            hosts: BTreeSet::from([host.clone()]),
        };
        if !subjects.contains(&listed) {
            subjects.push(listed);
        }
    }
    ListedRecord {
        subjects,
        as_of: event.created_at,
        event_id: event.id,
        host: host.clone(),
    }
}

/// One host's complete branch for the SELECTED relay-signed group records,
/// keyed on `d` by whatever `group_ids` resolves it to.
///
/// Replaces the fixed all-three-kinds listing: an app that renders a
/// directory asks for [`GroupRecord::Metadata`] alone and never pays a relay
/// for two lists it does not read.
///
/// `group_ids` of `None` builds a branch with NO `d` row at all -- every
/// group the host advertises among the selected records. That is the ABSENCE
/// of a constraint, and it is the only honest lowering of it: a `d` row
/// naming any specific set would ask the relay about those groups and no
/// others, which for a directory is indistinguishable from a host with no
/// groups.
///
/// A `Some` binding is embedded VERBATIM, for the same reason the listing
/// door always has: rewriting it would be the silent repin
/// `nmp_grammar::Derived` forbids.
///
/// `limit` is the ordinary NIP-01 `Filter::limit` and bounds THIS host's
/// branch alone -- never a global bound over the union, which
/// `nmp_grammar::LiveQuery` owns separately and which this crate never
/// invents.
#[must_use]
pub fn group_records_at(
    host: &RelayUrl,
    records: &BTreeSet<GroupRecord>,
    group_ids: Option<Binding>,
    limit: Option<usize>,
) -> Demand {
    debug_assert!(
        !records.is_empty(),
        "the facade proves the record selection is nonempty before lowering it"
    );
    explicit_at(
        host,
        Filter {
            kinds: Some(records.iter().map(|record| record.record_kind()).collect()),
            tags: group_ids
                .into_iter()
                .map(|binding| (join_key(), binding))
                .collect(),
            limit,
            ..Filter::default()
        },
    )
}

/// The first value of the first row named `name`, if the row has a value.
fn first_value(event: &Event, name: &str) -> Option<String> {
    event.tags.iter().find_map(|tag| {
        let row = tag.as_slice();
        (row.first().map(String::as_str) == Some(name))
            .then(|| row.get(1).map(String::to_string))
            .flatten()
    })
}

