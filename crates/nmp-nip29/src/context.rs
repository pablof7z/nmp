//! The `h` row: which group an event belongs to, and what may be said about
//! it (#977, #1033).
//!
//! This module owns NIP-29's group-context SEMANTICS and nothing else -- it
//! contextualizes an unsigned draft, validates an already-signed event, and
//! scopes one host's read branch to one group id. It holds no relay set, no
//! route, no engine, no signer and no intent itself: the app-facing door
//! that retains a scope and mints an opaque write lives elsewhere in this
//! same crate (`crate::RelayScope`/`crate::Group`), which need `nmp`'s
//! engine surface this module does not.
//!
//! Everything here is KIND-BLIND. It reads no kind, branches on no kind, and
//! privileges none: NIP-29 permits any kind to carry an `h` and live in a
//! group, and declaring a fixed content catalogue was the measured defect
//! #838 closed. The kinds NIP-29 itself DEFINES (9000-9022) live in
//! [`crate::operations`]; the kinds it defines for DESCRIBING a group
//! (39000/39001/39002) live in [`crate::discovery`].

use std::collections::BTreeSet;

use nmp_grammar::{Binding, Demand, EventBuilder, Filter, IndexedTagName};
use nostr::{Event, RelayUrl, Tag};

use crate::discovery::JOIN_KEY_TAG;

/// The row NIP-29 owns: which group an event belongs to.
const CONTEXT_TAG: char = 'h';

/// Typed refusal from group contextualization. Every variant is a caller
/// error at the door -- none of them is a relay rejection, and none of them
/// mutates, repairs or re-signs what the caller supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupContextError {
    /// An unsigned draft arrived already carrying an `h` row. The group id
    /// retained by the group is the only source of that row, so a caller's
    /// own is refused whether it matches this group or not -- the refusal is
    /// about WHO OWNS the tag, not about which value it happened to hold.
    CallerSuppliedContext,
    /// A read selection arrived already constraining `#h`. Same ownership
    /// rule from the read side: the retained group id is the sole semantic
    /// source of `h`, so a caller's own constraint is refused rather than
    /// silently overwritten (which would answer a question nobody asked).
    CallerSuppliedContextConstraint,
    /// A group-content read selection named one of NIP-29's own relay-signed
    /// group records (39000/39001/39002).
    ///
    /// Those records key themselves by `d`, never by `h`, so constraining
    /// them by this group's `h` row builds a filter no such event can match:
    /// the read would return nothing, forever, and an app could not tell that
    /// apart from a group whose relay published no roster (#1245). A door
    /// that returns nothing forever is worse than one that says no, so this
    /// says no -- the same ownership refusal
    /// [`Self::CallerSuppliedContextConstraint`] already makes, on the other
    /// axis.
    ///
    /// It is not a kind catalogue and it privileges no kind: which kinds live
    /// IN a group stays entirely the app's to choose. These three are not in
    /// the group, they are ABOUT it, and they are read through the group's
    /// own records door instead.
    RecordsAreNotContextScoped { kinds: BTreeSet<u16> },
    /// A write was composed for no group at all (#1281). An event with no
    /// `h` row is not in a group, so there is nothing for the door to
    /// contextualize and no honest route to mint. The refusal is at
    /// construction, which is what keeps every method below infallible with
    /// respect to the group set -- the same shape
    /// [`crate::GroupContextError`]'s relay-side counterpart
    /// (`crate::RelayScopeError::EmptyRelaySet`) already has.
    NoGroupNamed,
    /// A pre-signed event carries no `h` at all. Appending one would change
    /// the bytes and therefore the `EventId`, so this is a refusal rather
    /// than a repair.
    MissingContext { expected: BTreeSet<String> },
    /// A pre-signed event names a different SET of groups than the one
    /// publishing it. An event carrying `["h", "darkroom"]` published through
    /// `photographers` reports `found: {darkroom}`,
    /// `expected: {photographers}`; an event carrying a second `h` row beside
    /// the right one reports both in `found`, which is what the retired
    /// row-COUNT check could never say.
    MismatchedContext {
        found: BTreeSet<String>,
        expected: BTreeSet<String>,
    },
    /// A pre-signed event names the same group in more than one `h` row.
    ///
    /// The set of groups it claims is right, so this is not a
    /// [`Self::MismatchedContext`]; what is wrong is that the rows are not
    /// the rows this door would ever mint. Refusing keeps the pre-signed
    /// path exactly as strict as the unsigned one, which appends each id
    /// once and only once.
    RepeatedContext { repeated: BTreeSet<String> },
}

impl std::fmt::Display for GroupContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CallerSuppliedContext => write!(
                f,
                "the '{CONTEXT_TAG}' tag belongs to the group, not to the caller"
            ),
            Self::CallerSuppliedContextConstraint => write!(
                f,
                "the '#{CONTEXT_TAG}' constraint belongs to the group, not to the caller's \
                 selection"
            ),
            Self::RecordsAreNotContextScoped { kinds } => {
                let kinds: Vec<String> = kinds.iter().map(u16::to_string).collect();
                write!(
                    f,
                    "kinds {} are NIP-29's own relay-signed group records: they key themselves \
                     by '{JOIN_KEY_TAG}', never by '{CONTEXT_TAG}', so no such event could ever \
                     match a group-content read -- read them through the group's records door",
                    kinds.join(", ")
                )
            }
            Self::NoGroupNamed => write!(
                f,
                "a group write must name at least one group: an event with no \
                 '{CONTEXT_TAG}' row is not in a group at all"
            ),
            Self::MissingContext { expected } => write!(
                f,
                "a signed event carries no '{CONTEXT_TAG}' tag, so it is not in {}; \
                 appending one would change its event id",
                named(expected)
            ),
            Self::MismatchedContext { found, expected } => write!(
                f,
                "a signed event names {}, but it is being published through {}",
                named(found),
                named(expected)
            ),
            Self::RepeatedContext { repeated } => write!(
                f,
                "a signed event names {} in more than one '{CONTEXT_TAG}' row",
                named(repeated)
            ),
        }
    }
}

impl std::error::Error for GroupContextError {}

/// One host's complete read branch for one group: the app's OWN selection,
/// scoped by `#h = group_id` and pinned to exactly that host.
///
/// Which kinds live in a group is the app's to say; a fixed catalogue here is
/// precisely what #838 removed. An app selection that already constrains `#h`
/// is REFUSED rather than overwritten.
///
/// The one selection this door refuses on its content is NIP-29's own
/// relay-signed records (39000/39001/39002), because they are `d`-keyed and
/// an `h`-scoped filter over them matches nothing at all (#1245). That is a
/// refusal about which AXIS keys the event, not a catalogue of the group's
/// kinds: every other kind, defined by NIP-29, by another NIP, or by nothing,
/// passes through unread.
pub fn group_demand_at(
    host: &RelayUrl,
    group_id: &str,
    selection: Filter,
) -> Result<Demand, GroupContextError> {
    let mut selection = selection;
    if selection.tags.contains_key(&context_tag()) {
        return Err(GroupContextError::CallerSuppliedContextConstraint);
    }
    let records: BTreeSet<u16> = selection
        .kinds
        .iter()
        .flatten()
        .copied()
        .filter(|selected| crate::records::GroupRecord::of_kind(*selected).is_some())
        .collect();
    if !records.is_empty() {
        return Err(GroupContextError::RecordsAreNotContextScoped { kinds: records });
    }
    selection.tags.insert(
        context_tag(),
        Binding::Literal(BTreeSet::from([group_id.to_string()])),
    );
    Ok(crate::discovery::explicit_at(host, selection))
}

/// Append one `h` row per group in `group_ids` to a draft, refusing a draft
/// that already claims either tag this crate owns. Every other field and tag
/// survives verbatim, in the caller's own order; the appended rows follow the
/// set's own canonical order, so two callers naming the same groups in
/// different orders compose the identical bytes.
///
/// One group is the ONE-ELEMENT case and has no separate path: a write that
/// belongs to several groups at once -- a kind:30315 session status carrying
/// one `h` per room the session occupies (#1281) -- is the same call with a
/// larger set. There is no arity anywhere below this line.
///
/// This is the door every group write takes: NMP appends the rows, NMP signs,
/// and the app never spells an `h`.
///
/// The append happens on the BUILDER -- before the stamp/sign step -- so the
/// context tags are inside the bytes that get signed and are covered by the
/// id and the signature.
///
/// `group_ids` is proved nonempty by whoever formed it
/// ([`GroupContextError::NoGroupNamed`] is decided at that construction, not
/// here), exactly as [`crate::group_records_at`]'s record selection is.
pub fn contextualize(
    group_ids: &BTreeSet<String>,
    builder: EventBuilder,
) -> Result<EventBuilder, GroupContextError> {
    debug_assert!(
        !group_ids.is_empty(),
        "the door proves its group set is nonempty before contextualizing"
    );
    for tag in &builder.tags {
        if let Some(name) = tag.as_slice().first().map(String::as_str) {
            if name == CONTEXT_TAG.to_string() {
                return Err(GroupContextError::CallerSuppliedContext);
            }
        }
    }
    Ok(group_ids.iter().fold(builder, |builder, group_id| {
        builder.tag(
            Tag::parse([CONTEXT_TAG.to_string().as_str(), group_id.as_str()])
                .expect("'h' is a well-formed non-empty row"),
        )
    }))
}

/// Validate the `h` rows an ALREADY-SIGNED event carries. Nothing is
/// appended, nothing is re-signed, nothing is recomputed -- appending would
/// change the bytes and therefore the `EventId`, which is the whole reason an
/// app signs first.
///
/// The event's `h` rows must name EXACTLY `group_ids`, each once. Naming none
/// is [`GroupContextError::MissingContext`], naming a different set is
/// [`GroupContextError::MismatchedContext`], and naming the right set with a
/// row repeated is [`GroupContextError::RepeatedContext`] -- which is the
/// refusal that keeps this path exactly as strict as [`contextualize`], whose
/// rows a set can never repeat.
///
/// Set-shaped because the vocabulary is, not because a several-group
/// pre-signed door exists: there is none. What the set buys at the one-group
/// arity is a refusal that names the whole set it FOUND beside the whole set
/// it expected, so an event carrying a second `h` reports both rather than
/// only that something was wrong.
pub fn validate_context(
    group_ids: &BTreeSet<String>,
    event: &Event,
) -> Result<(), GroupContextError> {
    debug_assert!(
        !group_ids.is_empty(),
        "the door proves its group set is nonempty before validating"
    );
    let context = CONTEXT_TAG.to_string();
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut repeated: BTreeSet<String> = BTreeSet::new();
    let mut rows = 0usize;
    for tag in event.tags.iter() {
        let row = tag.as_slice();
        if row.first() != Some(&context) {
            continue;
        }
        rows += 1;
        let value = row
            .get(1)
            .map(String::to_string)
            .unwrap_or_else(String::new);
        if !found.insert(value.clone()) {
            repeated.insert(value);
        }
    }
    if rows == 0 {
        return Err(GroupContextError::MissingContext {
            expected: group_ids.clone(),
        });
    }
    if &found != group_ids {
        return Err(GroupContextError::MismatchedContext {
            found,
            expected: group_ids.clone(),
        });
    }
    if !repeated.is_empty() {
        return Err(GroupContextError::RepeatedContext { repeated });
    }
    Ok(())
}

fn context_tag() -> IndexedTagName {
    IndexedTagName::new(CONTEXT_TAG).expect("'h' is a single ASCII letter")
}

/// One group named as `group "x"`, several as `groups "x", "y"` -- so a
/// refusal message reads as a sentence at either arity without a caller
/// having to know which one it got.
fn named(group_ids: &BTreeSet<String>) -> String {
    let listed: Vec<String> = group_ids.iter().map(|id| format!("{id:?}")).collect();
    match listed.len() {
        1 => format!("group {}", listed[0]),
        _ => format!("groups {}", listed.join(", ")),
    }
}

