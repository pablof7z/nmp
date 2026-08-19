//! Profiles: the latest kind:0 per author, for the avatar on every row.
//!
//! ## What we wanted to write
//!
//! ```text
//! let people = engine.observe_authors_of(&feed)?;   // author -> latest kind:0
//! people.get(row.pubkey())                          // latest-wins, already reduced
//! ```
//!
//! ## What we wrote
//!
//! Two things, because neither one alone works.
//!
//! 1. [`profiles_of_authors`] -- a literal-author kind:0 query. The author set
//!    is `Binding::Literal(BTreeSet<String>)`: a set of HEX STRINGS. This app
//!    holds `PublicKey` values, so every call converts `PublicKey -> hex ->
//!    (engine) -> PublicKey`. `Binding` is the one place in the read grammar
//!    that is stringly typed; `Filter::authors` cannot take the decoded type
//!    the rest of the surface insists on.
//!
//!    Worse, a `LiveQuery` is a VALUE. Scrolling a feed reveals new authors,
//!    and there is no way to add them to a live observation -- the app cancels
//!    the profile subscription and opens a new one with a bigger literal set,
//!    discarding the engine's whole accumulated coverage for the authors that
//!    did not change.
//!
//! 2. [`profiles_of_query`] -- the version we actually wanted, and it IS
//!    expressible: `authors := Derived { inner: <the feed demand>, project:
//!    Selector::Authors }`. It reacts, so no cancel-and-reopen. The cost is
//!    that the feed's demand is now declared TWICE -- once as the feed's own
//!    observation and once nested inside the profile query -- and the app
//!    cannot say "the authors of the observation I already have open". Two
//!    identical inner demands, two atoms, two coverage keys, two sets of
//!    acquisition evidence describing the same wire work.
//!
//! ## And then the reduction
//!
//! Whichever query is used, what comes back is rows. kind:0 is replaceable,
//! so several rows can exist per author and the app picks latest-wins by
//! `created_at` itself ([`ProfileBook::apply`]). NMP has a replaceable
//! materializer concept (`ReplaceableMaterializerSpec`), and NIP-02 and NIP-29
//! both register one -- but no crate in this workspace owns kind:0, so there
//! is nothing to register and the reduction is the app's.
//!
//! ## And then the npub
//!
//! A person with no kind:0 still has to render as something. `nmp` re-exports
//! `decode_nostr_entity` for the paste direction; the encode direction exists
//! only as `nmp::Mention::render`, a content-composition trait method that
//! returns `"nostr:npub1..."`. [`npub`] strips the scheme back off. See
//! the `findings` entry on the bech32 encode direction.
//!
//! ## And then the JSON
//!
//! `nmp` re-exports `Event`, `EventId`, `Kind`, `PublicKey`, `RelayUrl`,
//! `Tag`, `Timestamp`, `UnsignedEvent` from `nostr` -- and no metadata value.
//! `nostr::Metadata` exists upstream and is not reachable. So this file
//! carries a `serde_json` dependency and its own struct to draw a name and an
//! avatar. Every Nostr app that renders a row needs this; none of them can get
//! it from NMP.

use std::collections::{BTreeMap, BTreeSet};

use nmp::{Binding, Demand, Derived, Filter, Frame, LiveQuery, PublicKey, Row, RowDelta, Selector};

/// NIP-01 kind:0. Written out because no crate in the workspace names it.
pub const METADATA_KIND: u16 = 0;

/// What a row actually needs to render a person.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Profile {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub picture: Option<String>,
    pub about: Option<String>,
    /// `created_at` of the kind:0 this was read from, kept only so the app can
    /// do the latest-wins reduction NMP does not do for kind:0.
    pub stamped_at: u64,
}

impl Profile {
    /// Parse a kind:0 body. Hand-rolled: see the module doc.
    #[must_use]
    pub fn read(row: &Row) -> Self {
        let value: serde_json::Value =
            serde_json::from_str(row.content()).unwrap_or(serde_json::Value::Null);
        let field = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        Self {
            name: field("name"),
            display_name: field("display_name").or_else(|| field("displayName")),
            picture: field("picture"),
            about: field("about"),
            stamped_at: row.created_at().as_secs(),
        }
    }

    /// The one string a row header shows.
    #[must_use]
    pub fn label(&self, author: PublicKey) -> String {
        self.display_name
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| npub_prefix(author))
    }
}

/// The literal-author profile query.
///
/// `authors` wants hex strings, so every `PublicKey` this app holds is
/// stringified on the way in.
#[must_use]
pub fn profiles_of_authors(authors: impl IntoIterator<Item = PublicKey>) -> LiveQuery {
    let authors: BTreeSet<String> = authors.into_iter().map(|key| key.to_hex()).collect();
    LiveQuery::single(Demand {
        selection: Filter {
            kinds: Some(BTreeSet::from([METADATA_KIND])),
            authors: Some(Binding::Literal(authors)),
            ..Filter::default()
        },
        ..Demand::default()
    })
}

/// The reactive profile query: kind:0 by whoever authored `of`'s rows.
///
/// This is the shape we wanted. It costs a second, structurally identical copy
/// of `of` -- the app cannot point at an observation it already holds.
#[must_use]
pub fn profiles_of_query(of: Demand) -> LiveQuery {
    LiveQuery::single(Demand {
        selection: Filter {
            kinds: Some(BTreeSet::from([METADATA_KIND])),
            authors: Some(Binding::Derived(Box::new(Derived {
                inner: of,
                project: Selector::Authors,
            }))),
            ..Filter::default()
        },
        ..Demand::default()
    })
}

/// The author-keyed projection the engine does not offer.
///
/// This is the `HashMap<PublicKey, Metadata>` every app builds, invalidated by
/// hand. It is here rather than in `RowTable` because the reduction is
/// kind-specific (latest-wins on a replaceable) and `RowTable` is kind-blind,
/// which is exactly the shape of the gap: the engine's row accumulation is
/// kind-blind too, and kind:0 needs a per-author fold nobody performs.
#[derive(Debug, Default)]
pub struct ProfileBook {
    by_author: BTreeMap<PublicKey, Profile>,
}

impl ProfileBook {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a delivered frame.
    ///
    /// `RowDelta::Removed` is the awkward one: it carries only an id, and this
    /// map is keyed by AUTHOR, so a removal cannot be applied without a second
    /// id->author index. We keep one. An app that skipped it would keep
    /// showing a profile the query no longer matches.
    pub fn apply(&mut self, frame: &Frame) {
        for delta in &frame.deltas {
            match delta {
                RowDelta::Added(row) | RowDelta::Updated(row) => {
                    let profile = Profile::read(row);
                    let author = row.pubkey();
                    match self.by_author.get(&author) {
                        Some(existing) if existing.stamped_at >= profile.stamped_at => {}
                        _ => {
                            self.by_author.insert(author, profile);
                        }
                    }
                }
                // Deliberately unhandled, and this is the finding: to honour a
                // removal we would need an `EventId -> PublicKey` index that
                // duplicates the row table's own keys. Every app either keeps
                // that second index or shows stale profiles. We show stale
                // profiles, and say so.
                RowDelta::Removed(_) | RowDelta::SourcesGrew { .. } => {}
            }
        }
    }

    #[must_use]
    pub fn get(&self, author: &PublicKey) -> Option<&Profile> {
        self.by_author.get(author)
    }

    /// The display string for a row header, falling back to a hex prefix.
    #[must_use]
    pub fn label(&self, author: PublicKey) -> String {
        self.by_author
            .get(&author)
            .map_or_else(|| Profile::default().label(author), |p| p.label(author))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_author.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_author.is_empty()
    }
}

/// The npub an app shows when a person has no kind:0.
///
/// The encode direction is reachable -- and only through `nmp::Mention`, a
/// trait whose stated job is inline CONTENT composition ("what the reader sees,
/// including NIP-21's `nostr:` scheme"). So a profile header gets its npub by
/// asking the content-composition door for a mention and slicing the scheme
/// off the front. `nmp` re-exports `decode_nostr_entity` as a first-class
/// function for the paste direction and nothing symmetrical for the render
/// direction.
#[must_use]
pub fn npub(author: PublicKey) -> String {
    let rendered = nmp::Mention::render(&author);
    rendered
        .strip_prefix("nostr:")
        .unwrap_or(&rendered)
        .to_string()
}

/// Truncated for a row header.
#[must_use]
pub fn npub_prefix(author: PublicKey) -> String {
    let npub = npub(author);
    let head: String = npub.chars().take(12).collect();
    format!("{head}\u{2026}")
}
