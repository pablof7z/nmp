//! Notifications: everything that points at me.
//!
//! ## What we wanted to write
//!
//! ```text
//! engine.observe(mentioning(me), Window::newest(50))
//! ```
//!
//! ## What we wrote
//!
//! [`mentions_of`], and the interesting part is a bug this app almost shipped.
//!
//! The obvious query is `#p = Reactive(ActivePubkey)`. `Filter::tags` accepts a
//! `Binding` in any position -- the grammar's own doc says so explicitly
//! ("legal in `authors` AND in any tag field (position-agnostic)"). So the
//! declaration is short and correct.
//!
//! But this app has TWO ACCOUNTS LIVE AT ONCE, and `IdentityField::ActivePubkey`
//! is singular. Notifications for the non-current account need a second
//! observation with `Binding::Literal([other_key_hex])` -- a structurally
//! DIFFERENT query for the same question, so the two accounts' notification
//! screens do not share a code path, an atom, or a coverage watermark. Switch
//! accounts and the reactive one re-roots (dropping every row and re-adding the
//! other account's, which is correct and free) while the literal one is now
//! pointed at the account that just became current, and the app has to swap
//! them.
//!
//! `IdentityField` is documented as extensible ("do not treat this as a closed
//! set to forbid growing"). What a two-account app needs is a reactive
//! reference to a NAMED session account, not to "the current one".
//!
//! ## Filtering out my own noise
//!
//! A notification list wants `#p = me AND author != me`. `Filter::authors` is a
//! `Binding` and `SetAlgebra::Diff` exists -- but `Diff` needs a first operand
//! to subtract FROM, and "everyone" is not expressible as a `Binding`. There is
//! no negation, only difference. So the app over-fetches and filters in the
//! view layer, which means the WINDOW is wrong too: `Window::Expandable {
//! initial: 50 }` delivers 50 rows of which some are the user's own, and
//! `request_rows` is the only way to get more. A windowed screen with a
//! view-layer predicate has no correct page size.

use std::collections::{BTreeMap, BTreeSet};

use nmp::{Binding, Demand, Filter, IdentityField, IndexedTagName, LiveQuery, PublicKey, Row};

/// Everything tagging the CURRENT account.
#[must_use]
pub fn mentions_of_current(kinds: impl IntoIterator<Item = u16>) -> LiveQuery {
    LiveQuery::single(Demand {
        selection: Filter {
            kinds: Some(kinds.into_iter().collect::<BTreeSet<u16>>()),
            tags: BTreeMap::from([(
                IndexedTagName::new('p').expect("'p' is an ASCII letter"),
                Binding::Reactive(IdentityField::ActivePubkey),
            )]),
            ..Filter::default()
        },
        ..Demand::default()
    })
}

/// Everything tagging one NAMED account -- the second account's notifications.
///
/// Structurally different from [`mentions_of_current`] for no reason the
/// protocol has: the binding is a literal instead of a reactive reference.
#[must_use]
pub fn mentions_of(who: PublicKey, kinds: impl IntoIterator<Item = u16>) -> LiveQuery {
    LiveQuery::single(Demand {
        selection: Filter {
            kinds: Some(kinds.into_iter().collect::<BTreeSet<u16>>()),
            tags: BTreeMap::from([(
                IndexedTagName::new('p').expect("'p' is an ASCII letter"),
                Binding::Literal(BTreeSet::from([who.to_hex()])),
            )]),
            ..Filter::default()
        },
        ..Demand::default()
    })
}

/// The view-layer predicate the grammar cannot express.
#[must_use]
pub fn is_someone_elses(row: &Row, me: PublicKey) -> bool {
    row.pubkey() != me
}

/// What kind of notification this row is, decided by kind number in the app.
///
/// `nmp-nip25` owns kind:7 and `nmp-nip18` owns 6/16, and neither exposes
/// "classify this row" -- both are write-side composers plus, in NIP-25's case,
/// a content vocabulary. So the notification list's own taxonomy is a `match`
/// on `u16` in an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    Reply,
    Reaction,
    Repost,
    Room,
    Other(u16),
}

#[must_use]
pub fn classify(row: &Row) -> NotificationKind {
    match row.kind().as_u16() {
        1 => NotificationKind::Reply,
        7 => NotificationKind::Reaction,
        6 | 16 => NotificationKind::Repost,
        9 | 11 | 12 => NotificationKind::Room,
        other => NotificationKind::Other(other),
    }
}
