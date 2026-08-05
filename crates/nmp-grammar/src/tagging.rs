//! The one door that turns "point at this entity" into rows (#1243).
//!
//! Every reference row on Nostr — `e`, `a`, `q`, `p`, and NIP-22's uppercase
//! `E`/`A`/`I`/`K`/`P` — says the same thing: *go look at this, here is where,
//! and here is who wrote it*. Before this door every composer built those rows
//! by hand, and an enumeration of all 33 of them across nmp, mosaico and
//! 29er-next found the relay hint filled in **one**, the author cell filled in
//! **one**, and a NIP-10 marker in **one** (a Swift file). So hints were not
//! sometimes wrong; nothing in the tree emitted one.
//!
//! ## What this door reads
//!
//! It reads the **target's own thread position**, from the target's own rows.
//! It never asks the caller what the relationship is, because every prior-art
//! library that took the relationship as a parameter shipped a bug for it:
//! amethyst#629 marked a direct reply-to-root `"reply"` instead of `"root"`
//! and broke thread reconstruction five hops deep, and NDK's two reply paths
//! disagree with each other about the same operation.
//!
//! Four wire shapes exist and all four must read to the same place
//! ([`ThreadPosition::read`]):
//!
//! 1. no `e` rows at all ⇒ the target IS the root. Certain.
//! 2. a `"root"`-marked `e` row ⇒ that names the root; the target is a parent.
//! 3. `e` rows but **no `"root"` marker** ⇒ the `"reply"`-marked row (else the
//!    last positional) is the root. This case is not optional: current
//!    rust-nostr emits exactly this when no root is passed, as do snstr and
//!    every pre-#629 Amethyst event still sitting on relays. Reading it wrong
//!    re-creates amethyst#629 from the reading side.
//! 4. positional only ⇒ NIP-10's stated ordering — first is root, last is
//!    parent.
//!
//! It also **tolerates** applesauce's duplicate-id form (two rows, one id,
//! `"root"` then `"reply"`) without calling it malformed. NMP does not emit
//! that: NIP-10 says *"A direct reply to the root of a thread should have a
//! single marked `e` tag of type `root`"*, its own git history converged on
//! that deliberately, and rust-nostr removed the double-marked form in v0.38.0
//! as redundant.
//!
//! ## What does NOT come through here
//!
//! NIP-29 rosters (`["p", <hex>, <role>]` — index 2 is a role, not a relay
//! hint), NIP-51 list entries, and NIP-9 deletion targets. The rule is that
//! this door is for rows saying *"go look at this, here's where"*. A roster
//! row, a list entry and a delete operand each say something else, and a relay
//! hint is meaningless in all three.

use std::collections::BTreeSet;

use nostr::{Event, EventId, Kind, PublicKey, RelayUrl, Tag};

/// NIP-10's kind for a text note, and therefore the one kind whose replies
/// thread through NIP-10's marked `e` rows instead of NIP-22's comment shape.
pub const TEXT_NOTE_KIND: u16 = 1;

/// NIP-22's fixed kind for a comment event. It lives here rather than in
/// `nmp-nip22` because [`reply_to`] is the door that mints it and this crate
/// is the door's home; `nmp-nip22` keeps the decoder, the thread demand and
/// the intent.
pub const COMMENT_KIND: u16 = 1111;

/// One entity a reference row can name, with every cell the row has slots
/// for. Deliberately all-optional except the identity: a row that knows the
/// author fills slot 4, a row that does not simply has fewer cells, and
/// nothing here invents a value to fill a slot with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pointer {
    /// The event's own id. `None` only for an addressable coordinate that
    /// pins no revision.
    pub event_id: Option<EventId>,
    /// `<kind>:<pubkey-hex>:<d>` when the entity is addressable.
    pub address: Option<String>,
    /// The `I`/`i` cell when the entity is not a Nostr event at all. Read and
    /// re-emitted verbatim — this crate is protocol-neutral and never parses
    /// the value, which is what keeps NIP-73 out of it.
    pub external: Option<String>,
    pub author: Option<PublicKey>,
    /// The `K`/`k` cell verbatim: a kind number for an event, and a namespace
    /// string for an external content id, which is why it is not a [`Kind`].
    pub kind: Option<String>,
    /// Where the row says to look. Verified provenance when it came from a
    /// `Row`'s observed sources, caller-stated when it came from
    /// [`TagOptions::from_relay`], and absent when neither knew.
    pub relay: Option<RelayUrl>,
}

impl Pointer {
    /// The relay cell, as a row renders it: NIP-10 and NIP-22 both use the
    /// empty string for "no hint" rather than a shorter row, because the
    /// cells after it are positional.
    fn relay_cell(&self) -> String {
        self.relay
            .as_ref()
            .map(RelayUrl::to_string)
            .unwrap_or_default()
    }

    fn with_relay_default(mut self, relay: Option<RelayUrl>) -> Self {
        if self.relay.is_none() {
            self.relay = relay;
        }
        self
    }
}

/// The modifiers a caller may add to one tagging call. Every one of them is
/// **additive and order-independent** — `target.uppercase().without_author()`
/// and `target.without_author().uppercase()` are the same value — so there is
/// no ordering to get wrong and no combination that means something a caller
/// did not say.
///
/// The defaults differ per relationship because the specs genuinely differ. A
/// reply carries the parent's `p` rows forward (NIP-10: *"the reply event's
/// `p` tags should contain all of E's `p` tags as well as the pubkey of the
/// event being replied to"*). A reaction does not (NIP-25: *"If a client
/// decides to include other `p` tags, which not recommended…"*), which is what
/// [`TagOptions::without_carried_mentions`] says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagOptions {
    root_scope: bool,
    without_carried_mentions: bool,
    without_author: bool,
    without_self: Option<PublicKey>,
    relay: Option<RelayUrl>,
}

impl TagOptions {
    /// Emit NIP-22's uppercase ROOT SCOPE (`E`/`A`/`I` + `K` + `P`) instead of
    /// the lowercase rows naming the target itself.
    ///
    /// Uppercase is how NIP-22 states importance, and it is where everyone who
    /// tried this shipped a dated correction: NDK's `31f7e3bc` reads *"NIP-22
    /// uses uppercase tags (E/A) to indicate importance instead of marker
    /// parameters."* A root-scope row therefore never carries a
    /// `"root"`/`"reply"` marker in any position.
    pub fn uppercase(mut self) -> Self {
        self.root_scope = true;
        self
    }

    /// Do not carry the target's own `p` rows forward. What a quote
    /// (NIP-18) and a reaction (NIP-25) want; a reply does not use it.
    pub fn without_carried_mentions(mut self) -> Self {
        self.without_carried_mentions = true;
        self
    }

    /// Suppress the companion `p` row naming the target's author.
    ///
    /// It suppresses ONLY the `p` row. The author stays in the reference
    /// row's own author slot, because that slot is an outbox hint — it tells a
    /// reader whose relays to look at — and is not a notification. Dropping it
    /// would remove the ability to find the event, which is not what declining
    /// to notify someone means.
    pub fn without_author(mut self) -> Self {
        self.without_author = true;
        self
    }

    /// Drop `p` rows naming `pubkey` — normally the composing account, so a
    /// reply does not notify its own author.
    ///
    /// It takes the key because [`crate::EventBuilder`] structurally cannot
    /// carry an author: identity arrives from [`crate::WriteIntent`] at
    /// publish, so a builder has no way to know whose key to drop.
    ///
    /// Stripping self-`p` at signing time instead was considered and
    /// **rejected**. mosaico's four `allow_self_tagging()` call sites are all
    /// NIP-29 membership rows where the `p` row is the OPERAND OF THE VERB:
    /// blanket stripping turns "add me to this group" into a silently empty
    /// kind:9000. A `p` row is not always a notification, and nothing
    /// downstream of the builder can tell the difference from the bytes.
    pub fn without_self(mut self, pubkey: PublicKey) -> Self {
        self.without_self = Some(pubkey);
        self
    }

    /// State the relay hint instead of taking the target's own. What an app
    /// that knows better than the observed sources uses.
    pub fn from_relay(mut self, relay: RelayUrl) -> Self {
        self.relay = Some(relay);
        self
    }

    /// The stated relay hint, for an implementation outside this crate that
    /// builds its own rows because it describes an entity by its parts rather
    /// than holding the event.
    pub fn relay_hint(&self) -> Option<&RelayUrl> {
        self.relay.as_ref()
    }

    /// Whether the companion `p`/`P` row is declined -- see
    /// [`Self::without_author`].
    pub fn suppresses_author(&self) -> bool {
        self.without_author
    }

    /// Whether `pubkey` survives [`Self::without_self`].
    pub fn keeps_pubkey(&self, pubkey: &PublicKey) -> bool {
        self.keeps(pubkey)
    }

    fn keeps(&self, pubkey: &PublicKey) -> bool {
        self.without_self.as_ref() != Some(pubkey)
    }

    /// Union two modifier sets. Additive means exactly this: a suppression
    /// either set states is stated, and no combination can un-say something.
    /// It is what makes the vocabulary order-independent even when one call
    /// nests inside another verb that adds its own.
    fn union(&self, other: &Self) -> Self {
        Self {
            root_scope: self.root_scope || other.root_scope,
            without_carried_mentions: self.without_carried_mentions
                || other.without_carried_mentions,
            without_author: self.without_author || other.without_author,
            without_self: self.without_self.or(other.without_self),
            relay: self.relay.clone().or_else(|| other.relay.clone()),
        }
    }
}

/// Where an event sits in its thread, as its own rows say — never as a caller
/// claims. The [`read`](Self::read) rules are the whole reason this type
/// exists rather than a pair of parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPosition {
    /// The thread's root. `None` means the event has no `e`/`E`/`A`/`I` rows
    /// at all, so the event IS the root.
    pub root: Option<Pointer>,
    /// The event's direct parent, when it names one distinct from the root.
    pub parent: Option<Pointer>,
}

impl ThreadPosition {
    /// Read `event`'s own position from its own rows. Every shape on the wire
    /// that means the same place must land here identically — that is the
    /// property `every_wire_reply_shape_reads_to_the_same_thread_position`
    /// falsifies.
    pub fn read(event: &Event) -> Self {
        if let Some(position) = Self::read_comment_scope(event) {
            return position;
        }
        Self::read_nip10(event)
    }

    /// A NIP-22 comment states its root scope in uppercase and never with a
    /// marker, so its root is read from `E`/`A`/`I` + `K` + `P` directly and
    /// its parent from the lowercase mirror.
    fn read_comment_scope(event: &Event) -> Option<Self> {
        let rows = rows_of(event);
        // Any of the three uppercase root letters means the event states its
        // root scope NIP-22's way. An external content id has only `I`, and
        // an addressable root may have `A` with no `E` at all.
        if find(&rows, "E").is_none() && find(&rows, "A").is_none() && find(&rows, "I").is_none() {
            return None;
        }
        let root = Pointer {
            event_id: find(&rows, "E")
                .and_then(|row| cell(row, 1))
                .and_then(|hex| EventId::from_hex(hex).ok()),
            address: find(&rows, "A")
                .and_then(|row| cell(row, 1))
                .map(String::from),
            external: find(&rows, "I")
                .and_then(|row| cell(row, 1))
                .map(String::from),
            author: find(&rows, "P").and_then(|row| pubkey_at(row, 1)),
            kind: find(&rows, "K")
                .and_then(|row| cell(row, 1))
                .map(String::from),
            relay: find(&rows, "E")
                .or_else(|| find(&rows, "A"))
                .and_then(|row| relay_at(row, 2)),
        };
        let parent = find(&rows, "e")
            .and_then(pointer_from_e_row)
            .filter(|parent| parent.event_id != root.event_id);
        Some(Self {
            root: Some(root),
            parent,
        })
    }

    /// NIP-10's four reading cases, in the order the spec and the deployed
    /// corpus require.
    fn read_nip10(event: &Event) -> Self {
        let rows = rows_of(event);
        let e_rows: Vec<&[String]> = rows
            .iter()
            .copied()
            .filter(|row| cell(row, 0) == Some("e"))
            .collect();

        // Case 1: no `e` rows at all -- the event IS the root. Certain.
        if e_rows.is_empty() {
            return Self {
                root: None,
                parent: None,
            };
        }

        let marked = |marker: &str| -> Option<&[String]> {
            e_rows
                .iter()
                .copied()
                .find(|row| cell(row, 3) == Some(marker))
        };

        // Case 2: a `"root"` marker names the root outright. The `"reply"`
        // marker, when present, names the parent -- unless it repeats the
        // root's own id, which is applesauce's duplicate-id form: two rows,
        // one id, `"root"` then `"reply"`. That is a direct reply to the
        // root, semantically identical to the single-marked form, so it must
        // read to the same place rather than being called malformed.
        if let Some(root_row) = marked("root") {
            let root = pointer_from_e_row(root_row);
            let parent = marked("reply")
                .and_then(pointer_from_e_row)
                .filter(|parent| Some(&parent.event_id) != root.as_ref().map(|r| &r.event_id));
            return Self { root, parent };
        }

        // Case 3: `e` rows but NO `"root"` marker. The `"reply"`-marked row is
        // the root. Current rust-nostr emits exactly this shape when no root
        // is passed, as do snstr and every pre-#629 Amethyst event still on
        // relays; treating it as a parent instead re-creates amethyst#629
        // from the reading side.
        if let Some(reply_row) = marked("reply") {
            return Self {
                root: pointer_from_e_row(reply_row),
                parent: None,
            };
        }

        // Case 4: positional only -- NIP-10's stated ordering. One row is a
        // direct reply to the root; two or more make the first the root and
        // the last the parent.
        let first = e_rows.first().copied().and_then(pointer_from_e_row);
        if e_rows.len() == 1 {
            return Self {
                root: first,
                parent: None,
            };
        }
        Self {
            root: first,
            parent: e_rows.last().copied().and_then(pointer_from_e_row),
        }
    }
}

/// A thing that can be pointed at. Grammar defines the trait and the protocol
/// crate implements it, which is the seam that lets a NIP-73 external content
/// id be a reply target without this crate ever naming a NIP — the pattern
/// #1059 used for routing facts. It names no NIP itself.
pub trait RootScope {
    /// NIP-22's uppercase root-scope rows (`E`/`A`/`I` + `K` + `P`) naming the
    /// THREAD ROOT this target sits under — which is the target itself when
    /// the target is a root.
    fn root_rows(&self, options: &TagOptions) -> Vec<Tag>;

    /// The lowercase rows naming this target as the thing being replied to.
    fn parent_rows(&self, options: &TagOptions) -> Vec<Tag>;

    /// The kind of the entity being pointed at. `None` for an external
    /// content id, which is not a Nostr event and has no kind. Read by
    /// [`reply_to`] and by NIP-18's repost door; never read to decide root
    /// versus parent, which is [`ThreadPosition`]'s job alone.
    fn entity_kind(&self) -> Option<Kind>;

    /// The kind a reply to this target takes: NIP-10 threading for a text
    /// note, NIP-22's comment for everything else — including an external
    /// content id, which has no kind at all.
    fn reply_kind(&self) -> Kind {
        match self.entity_kind() {
            Some(kind) if kind == Kind::from(TEXT_NOTE_KIND) => Kind::from(TEXT_NOTE_KIND),
            _ => Kind::from(COMMENT_KIND),
        }
    }
}

/// A target plus the modifiers one tagging call applies to it.
pub struct Tagged<'a, T: ?Sized> {
    target: &'a T,
    options: TagOptions,
}

/// The modifier vocabulary, available on any target and on an
/// already-modified one so the calls chain in any order.
///
/// Blanket-implemented: implementing [`RootScope`] is the whole cost of being
/// taggable, and no protocol crate ever writes one of these methods.
pub trait Modifiers: RootScope + Sized {
    /// See [`TagOptions::uppercase`].
    fn uppercase(&self) -> Tagged<'_, Self> {
        Tagged {
            target: self,
            options: TagOptions::default().uppercase(),
        }
    }
    /// See [`TagOptions::without_carried_mentions`].
    fn without_carried_mentions(&self) -> Tagged<'_, Self> {
        Tagged {
            target: self,
            options: TagOptions::default().without_carried_mentions(),
        }
    }
    /// See [`TagOptions::without_author`].
    fn without_author(&self) -> Tagged<'_, Self> {
        Tagged {
            target: self,
            options: TagOptions::default().without_author(),
        }
    }
    /// See [`TagOptions::without_self`].
    fn without_self(&self, pubkey: PublicKey) -> Tagged<'_, Self> {
        Tagged {
            target: self,
            options: TagOptions::default().without_self(pubkey),
        }
    }
    /// See [`TagOptions::from_relay`].
    ///
    /// `from_relay` names where the HINT comes from, not a conversion of a
    /// relay into something else, so clippy's `from_*`-is-a-constructor
    /// convention does not apply and the ruled modifier vocabulary keeps its
    /// spelling.
    #[allow(clippy::wrong_self_convention)]
    fn from_relay(&self, relay: RelayUrl) -> Tagged<'_, Self> {
        Tagged {
            target: self,
            options: TagOptions::default().from_relay(relay),
        }
    }
}

impl<T: RootScope + Sized> Modifiers for T {}

impl<'a, T: RootScope> Tagged<'a, T> {
    /// See [`TagOptions::uppercase`].
    pub fn uppercase(mut self) -> Self {
        self.options = self.options.uppercase();
        self
    }
    /// See [`TagOptions::without_carried_mentions`].
    pub fn without_carried_mentions(mut self) -> Self {
        self.options = self.options.without_carried_mentions();
        self
    }
    /// See [`TagOptions::without_author`].
    pub fn without_author(mut self) -> Self {
        self.options = self.options.without_author();
        self
    }
    /// See [`TagOptions::without_self`].
    pub fn without_self(mut self, pubkey: PublicKey) -> Self {
        self.options = self.options.without_self(pubkey);
        self
    }
    /// See [`TagOptions::from_relay`].
    pub fn from_relay(mut self, relay: RelayUrl) -> Self {
        self.options = self.options.from_relay(relay);
        self
    }
}

/// Everything [`crate::EventBuilder::tag`] accepts.
///
/// Three implementations and no more: a raw [`Tag`] (the deliberate exact
/// escape hatch that has always been there), a bare target, and a target
/// carrying modifiers. Because all three land in the same function, dedup and
/// hint-filling behave identically on every internal path — which is the
/// caution NDK's two divergent reply branches supply.
pub trait TagRows {
    fn tag_rows(self) -> Vec<Tag>;
}

impl TagRows for Tag {
    fn tag_rows(self) -> Vec<Tag> {
        vec![self]
    }
}

impl<T: RootScope + ?Sized> TagRows for &T {
    fn tag_rows(self) -> Vec<Tag> {
        self.parent_rows(&TagOptions::default())
    }
}

impl<T: RootScope> TagRows for Tagged<'_, T> {
    fn tag_rows(self) -> Vec<Tag> {
        if self.options.root_scope {
            self.target.root_rows(&self.options)
        } else {
            self.target.parent_rows(&self.options)
        }
    }
}

/// A modified target is still a target, so it can be handed to a schema's own
/// reply verb (`nmp_nipc7::chat_reply`) or to [`reply_to`] exactly like an
/// unmodified one, and the modifiers survive whatever those verbs add.
impl<T: RootScope> RootScope for Tagged<'_, T> {
    fn root_rows(&self, options: &TagOptions) -> Vec<Tag> {
        self.target.root_rows(&self.options.union(options))
    }

    fn parent_rows(&self, options: &TagOptions) -> Vec<Tag> {
        self.target.parent_rows(&self.options.union(options))
    }

    fn entity_kind(&self) -> Option<Kind> {
        self.target.entity_kind()
    }
}

/// Compose a reply to `target`.
///
/// Two-way and no more: a text note threads through NIP-10, and everything
/// else — every other kind, and an external content id, which has no kind —
/// becomes a NIP-22 comment. There is no growing match and no registry,
/// because a schema with its own reply convention offers **its own verb**
/// (`nmp_nipc7::chat_reply`) rather than an arm in a dispatcher. Kind:9 is the
/// worked example: NIP-29 clients MUST only fetch kind 9, so a 1111 reply in a
/// group would be invisible to every one of them.
///
/// The split reads the TARGET's kind, never the composing kind, and the
/// root/parent determination underneath reads neither — see
/// [`ThreadPosition`].
pub fn reply_to<T: RootScope>(target: &T) -> crate::EventBuilder {
    let kind = target.reply_kind();
    if kind == Kind::from(TEXT_NOTE_KIND) {
        crate::EventBuilder::new(kind).tag(target)
    } else {
        crate::EventBuilder::new(kind)
            .tag(target.uppercase())
            .tag(target)
    }
}

// ---------------------------------------------------------------------------
// Row construction, shared by every implementation so no two paths can drift.
// ---------------------------------------------------------------------------

fn rows_of(event: &Event) -> Vec<&[String]> {
    event.tags.iter().map(|tag| tag.as_slice()).collect()
}

fn cell(row: &[String], index: usize) -> Option<&str> {
    row.get(index).map(String::as_str).filter(|s| !s.is_empty())
}

fn find<'a>(rows: &[&'a [String]], name: &str) -> Option<&'a [String]> {
    rows.iter().copied().find(|row| cell(row, 0) == Some(name))
}

fn pubkey_at(row: &[String], index: usize) -> Option<PublicKey> {
    cell(row, index).and_then(|hex| PublicKey::from_hex(hex).ok())
}

fn relay_at(row: &[String], index: usize) -> Option<RelayUrl> {
    cell(row, index).and_then(|url| RelayUrl::parse(url).ok())
}

/// A NIP-10 marked `e` row is `["e", <id>, <relay>, <marker>, <pubkey>]`;
/// an unmarked one stops earlier. Slot 4 is the author either way, and slot 3
/// is only ever a marker.
fn pointer_from_e_row(row: &[String]) -> Option<Pointer> {
    let event_id = EventId::from_hex(cell(row, 1)?).ok()?;
    Some(Pointer {
        event_id: Some(event_id),
        address: None,
        external: None,
        author: pubkey_at(row, 4).or_else(|| {
            // NIP-22's lowercase mirror has no marker slot, so its author sits
            // at index 3 where NIP-10 puts the marker. A marker is never valid
            // hex, so trying index 3 cannot misread a NIP-10 row.
            pubkey_at(row, 3)
        }),
        kind: None,
        relay: relay_at(row, 2),
    })
}

/// The carried `p` rows: every pubkey the target itself notified, minus the
/// declined key, deduplicated. Carrying these forward is NIP-10's own
/// instruction (*"the reply event's `p` tags should contain all of E's `p`
/// tags as well as the pubkey of the event being replied to"*).
fn carried_pubkeys(event: &Event, options: &TagOptions) -> Vec<PublicKey> {
    if options.without_carried_mentions {
        return Vec::new();
    }
    let mut seen = BTreeSet::new();
    let mut carried = Vec::new();
    for row in rows_of(event) {
        if cell(row, 0) != Some("p") {
            continue;
        }
        // A NIP-29 roster row (`["p", <hex>, <role>]`) puts a ROLE where a
        // pointer row puts a relay hint. It is not a mention and does not
        // come through this door, so it is not carried out of one either.
        let Some(pubkey) = pubkey_at(row, 1) else {
            continue;
        };
        if options.keeps(&pubkey) && seen.insert(pubkey) {
            carried.push(pubkey);
        }
    }
    carried
}

/// The author `p` row plus the carried ones, in that order, deduplicated
/// across both. One function, so every caller dedupes identically.
///
/// The hint cell is filled ONLY from a stated [`TagOptions::from_relay`],
/// never from where the target event was observed — and that asymmetry with
/// the `e` row above it is deliberate. An observed source is a verified fact
/// about where THAT EVENT is; it establishes nothing about where to find its
/// AUTHOR, which is an outbox fact (NIP-65) this crate cannot reach. Filling
/// it from the event's source would be a guess wearing a verified fact's
/// clothes. Where person hints come from is open on purpose (#1243).
fn person_rows(author: Option<PublicKey>, carried: &[PublicKey], options: &TagOptions) -> Vec<Tag> {
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    let relay = options
        .relay
        .as_ref()
        .map(RelayUrl::to_string)
        .unwrap_or_default();
    let push = |pubkey: PublicKey, seen: &mut BTreeSet<PublicKey>, rows: &mut Vec<Tag>| {
        if !options.keeps(&pubkey) || !seen.insert(pubkey) {
            return;
        }
        rows.push(row(["p", &pubkey.to_hex(), &relay]));
    };
    if !options.without_author {
        if let Some(author) = author {
            push(author, &mut seen, &mut rows);
        }
    }
    for pubkey in carried {
        push(*pubkey, &mut seen, &mut rows);
    }
    rows
}

/// Build one row, trimming the trailing empty cells a row does not need. A
/// cell in the middle stays even when empty, because everything after it is
/// positional.
fn row<const N: usize>(cells: [&str; N]) -> Tag {
    let mut cells: Vec<&str> = cells.to_vec();
    while cells.len() > 2 && cells.last().is_some_and(|last| last.is_empty()) {
        cells.pop();
    }
    Tag::parse(cells).expect("a reference row always has a non-empty first cell")
}

/// The lowercase rows naming `target` as a parent, in whichever dialect the
/// TARGET's own kind threads in.
///
/// The dialect follows the target, never the event being composed: a text
/// note's replies thread through NIP-10's marked rows, and everything else
/// threads through NIP-22's `e`/`k`/`p` mirror. That is the same fact
/// [`reply_to`] splits on, read once.
pub fn event_parent_rows(
    event: &Event,
    sources: Option<RelayUrl>,
    options: &TagOptions,
) -> Vec<Tag> {
    let position = ThreadPosition::read(event);
    let hint = options.relay.clone().or(sources);
    let target = Pointer {
        event_id: Some(event.id),
        address: addressable_coordinate(event),
        external: None,
        author: Some(event.pubkey),
        kind: Some(event.kind.as_u16().to_string()),
        relay: hint.clone(),
    };
    let carried = carried_pubkeys(event, options);

    let mut rows = Vec::new();
    if event.kind == Kind::from(TEXT_NOTE_KIND) {
        // NIP-10. A direct reply to a root gets a SINGLE `"root"`-marked row:
        // the spec says so twice, its git history converged on it
        // deliberately, quartz/NDK/welshman/Damus/Primal/Snort/nostter all do
        // it, and rust-nostr deleted the double-marked form in v0.38.0 as
        // redundant.
        let root = position.root.clone().unwrap_or_else(|| target.clone());
        rows.push(marked_e_row(&root, "root"));
        if position.root.is_some() {
            rows.push(marked_e_row(&target, "reply"));
        }
    } else {
        // NIP-22's lowercase mirror: the parent, its kind, its author. Four
        // cells, not two -- NIP-22 defines `["e", <id>, <relay>, <pubkey>]`,
        // and the old hand-built composer emitted two of them six times in
        // one file.
        rows.extend(scope_rows(&target, false));
    }
    rows.extend(person_rows(target.author, &carried, options));
    rows
}

/// The rows naming `event` AS AN ENTITY — no thread reading at all.
///
/// This is what a relationship that points at a thing rather than at a
/// position in a conversation needs: a repost names the note it reposts, and
/// a quote names the note it quotes. Running those through the threading
/// dialect would be actively wrong, not merely noisy: a text note that is
/// itself a reply threads as TWO `e` rows (root then reply), and NIP-18
/// readers take the first `e` as the reposted event — so a repost of a reply
/// would repost the thread's root instead of the note the user chose.
///
/// The cells are filled by the same code as every other pointer, so the
/// letter, the hint, the author slot and the companion `p` row cannot drift
/// between a repost and a reply.
pub fn entity_rows(event: &Event, sources: Option<RelayUrl>, options: &TagOptions) -> Vec<Tag> {
    let hint = options.relay.clone().or(sources);
    let target = Pointer {
        event_id: Some(event.id),
        address: addressable_coordinate(event),
        external: None,
        author: Some(event.pubkey),
        kind: Some(event.kind.as_u16().to_string()),
        relay: hint,
    };
    let mut rows = scope_rows(&target, false);
    rows.extend(person_rows(
        target.author,
        &carried_pubkeys(event, options),
        options,
    ));
    rows
}

/// NIP-22's uppercase root-scope rows for the thread `event` sits in.
pub fn event_root_rows(event: &Event, sources: Option<RelayUrl>, options: &TagOptions) -> Vec<Tag> {
    let hint = options.relay.clone().or(sources);
    let root = ThreadPosition::read(event)
        .root
        .map(|root| {
            // A NIP-10 thread is kind:1 by definition, so a root reached
            // through marked rows carries that kind even though the row
            // itself has no slot for one.
            Pointer {
                kind: root.kind.or_else(|| Some(event.kind.as_u16().to_string())),
                ..root
            }
        })
        .unwrap_or_else(|| Pointer {
            event_id: Some(event.id),
            address: addressable_coordinate(event),
            external: None,
            author: Some(event.pubkey),
            kind: Some(event.kind.as_u16().to_string()),
            relay: None,
        })
        .with_relay_default(hint);

    let mut rows = scope_rows(&root, true);
    if !options.without_author {
        if let Some(author) = root.author.filter(|author| options.keeps(author)) {
            rows.push(row(["P", &author.to_hex()]));
        }
    }
    rows
}

/// The `E`/`A` + `K` pair (uppercase) or `e`/`a` + `k` pair (lowercase) for
/// one pointer. Never a marker in either case: NIP-22 states importance
/// with case, which is exactly the correction NDK's `31f7e3bc` shipped.
fn scope_rows(pointer: &Pointer, uppercase: bool) -> Vec<Tag> {
    let (e, a, i, k) = if uppercase {
        ("E", "A", "I", "K")
    } else {
        ("e", "a", "i", "k")
    };
    let relay = pointer.relay_cell();
    let author = pointer
        .author
        .map(|author| author.to_hex())
        .unwrap_or_default();
    let mut rows = Vec::new();
    if let Some(external) = &pointer.external {
        rows.push(row([i, external]));
    }
    if let Some(address) = &pointer.address {
        rows.push(row([a, address, &relay, &author]));
    }
    if let Some(event_id) = pointer.event_id {
        // NIP-22: "when the parent event is replaceable or addressable, also
        // include an `e` tag referencing its id" -- a coordinate alone does
        // not pin a revision.
        rows.push(row([e, &event_id.to_hex(), &relay, &author]));
    }
    if let Some(kind) = &pointer.kind {
        rows.push(row([k, kind]));
    }
    // The `p`/`P` companion is emitted by `person_rows`/`event_root_rows`,
    // which own dedup for every path so no two callers can drift.
    rows
}

fn marked_e_row(pointer: &Pointer, marker: &str) -> Tag {
    let relay = pointer.relay_cell();
    let author = pointer
        .author
        .map(|author| author.to_hex())
        .unwrap_or_default();
    let id = pointer.event_id.map(|id| id.to_hex()).unwrap_or_default();
    row(["e", &id, &relay, marker, &author])
}

/// A `Pointer` naming an external content id names nothing else, so a
/// pointer-row author slot and a relay hint have nothing to attach to.
fn addressable_coordinate(event: &Event) -> Option<String> {
    if !event.kind.is_addressable() {
        return None;
    }
    let identifier = event
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .find(|row| row.first().map(String::as_str) == Some("d"))
        .and_then(|row| row.get(1).cloned())
        .unwrap_or_default();
    Some(format!(
        "{}:{}:{identifier}",
        event.kind.as_u16(),
        event.pubkey.to_hex()
    ))
}

/// Grammar's own implementation over a bare signed event. A `Row` adds one
/// thing to this — the relay hint its observed sources supply — so the facade
/// implements the trait by delegating here rather than restating any of it.
impl RootScope for Event {
    fn root_rows(&self, options: &TagOptions) -> Vec<Tag> {
        event_root_rows(self, None, options)
    }

    fn parent_rows(&self, options: &TagOptions) -> Vec<Tag> {
        event_parent_rows(self, None, options)
    }

    fn entity_kind(&self) -> Option<Kind> {
        Some(self.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder as NostrBuilder, Keys, Timestamp};

    fn signed(kind: u16, tags: Vec<Tag>) -> Event {
        let keys = Keys::generate();
        NostrBuilder::new(Kind::from(kind), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(1_700_000_000))
            .sign_with_keys(&keys)
            .expect("test event signs")
    }

    fn id(byte: u8) -> EventId {
        EventId::from_slice(&[byte; 32]).unwrap()
    }

    fn rows(tags: &[Tag]) -> Vec<Vec<String>> {
        tags.iter().map(|tag| tag.as_slice().to_vec()).collect()
    }

    /// The highest-value test in the design. Five wire shapes that all mean
    /// "this event is a reply whose thread root is R" must read to the SAME
    /// (root, parent) pair -- including applesauce's duplicate-id form, which
    /// is tolerated rather than called malformed.
    #[test]
    fn every_wire_reply_shape_reads_to_the_same_thread_position() {
        let root = id(1);
        let parent = id(2);

        // Case 2: marked root + marked reply.
        let marked = signed(
            1,
            vec![
                Tag::parse(["e", &root.to_hex(), "", "root"]).unwrap(),
                Tag::parse(["e", &parent.to_hex(), "", "reply"]).unwrap(),
            ],
        );
        // Case 4: positional -- first is root, last is parent.
        let positional = signed(
            1,
            vec![
                Tag::parse(["e", &root.to_hex()]).unwrap(),
                Tag::parse(["e", &parent.to_hex()]).unwrap(),
            ],
        );
        for event in [&marked, &positional] {
            let position = ThreadPosition::read(event);
            assert_eq!(
                position.root.as_ref().and_then(|r| r.event_id),
                Some(root),
                "root must read identically from every shape"
            );
            assert_eq!(
                position.parent.as_ref().and_then(|r| r.event_id),
                Some(parent)
            );
        }

        // Three shapes that all mean "a DIRECT reply to the root": the single
        // marked row NMP emits, the `"reply"`-only shape current rust-nostr
        // and snstr emit, and applesauce's duplicate-id pair.
        let single_marked = signed(
            1,
            vec![Tag::parse(["e", &root.to_hex(), "", "root"]).unwrap()],
        );
        let reply_marker_only = signed(
            1,
            vec![Tag::parse(["e", &root.to_hex(), "", "reply"]).unwrap()],
        );
        let applesauce_duplicate = signed(
            1,
            vec![
                Tag::parse(["e", &root.to_hex(), "", "root"]).unwrap(),
                Tag::parse(["e", &root.to_hex(), "", "reply"]).unwrap(),
            ],
        );
        let single_positional = signed(1, vec![Tag::parse(["e", &root.to_hex()]).unwrap()]);
        for event in [
            &single_marked,
            &reply_marker_only,
            &applesauce_duplicate,
            &single_positional,
        ] {
            let position = ThreadPosition::read(event);
            assert_eq!(position.root.as_ref().and_then(|r| r.event_id), Some(root));
            assert_eq!(
                position.parent, None,
                "a direct reply to the root names no separate parent"
            );
        }

        // Case 3 is load-bearing and this is where it earns its keep. With
        // `e` rows but NO `"root"` marker, the `"reply"`-marked row names the
        // ROOT -- it is the only row on the event that claims a thread
        // position at all, and the unmarked sibling is a plain mention. Read
        // as NIP-10 positional instead (first is root), the mention becomes
        // the thread and every reply below this one is filed under the wrong
        // conversation: amethyst#629 from the reading side.
        let mention = id(9);
        let no_root_marker = signed(
            1,
            vec![
                Tag::parse(["e", &mention.to_hex()]).unwrap(),
                Tag::parse(["e", &root.to_hex(), "", "reply"]).unwrap(),
            ],
        );
        let position = ThreadPosition::read(&no_root_marker);
        assert_eq!(
            position.root.as_ref().and_then(|r| r.event_id),
            Some(root),
            "the reply-marked row names the root when nothing is marked root"
        );
        assert_eq!(position.parent, None);
    }

    /// An event with no `e` rows IS the root, certainly -- and a reply to it
    /// therefore emits ONE `"root"`-marked row rather than a root/reply pair.
    #[test]
    fn a_root_is_tagged_with_a_single_root_marked_row() {
        let note = signed(1, vec![]);
        assert_eq!(ThreadPosition::read(&note).root, None);

        let built = crate::EventBuilder::new(Kind::from(1)).tag(&note);
        let emitted = rows(&built.tags);
        assert_eq!(
            emitted[0],
            vec![
                "e".to_string(),
                note.id.to_hex(),
                String::new(),
                "root".to_string(),
                note.pubkey.to_hex()
            ]
        );
        assert!(
            !emitted.iter().any(|row| row.contains(&"reply".to_string())),
            "a direct reply to a root carries a single marked row, not two"
        );
    }

    /// Root-versus-reply tracks the TARGET's actual thread position, and the
    /// same target tagged from different composers yields byte-identical
    /// pointer rows. This is what catches amethyst#629's inversion and NDK's
    /// split-path divergence at once.
    #[test]
    fn same_target_yields_same_rows_regardless_of_caller() {
        let root = signed(1, vec![]);
        let reply = signed(
            1,
            vec![Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap()],
        );

        let through_reply_to = reply_to(&reply);
        let hand_built = crate::EventBuilder::new(Kind::from(1)).tag(&reply);
        let through_a_reaction = crate::EventBuilder::new(Kind::from(7)).tag(&reply);
        assert_eq!(rows(&through_reply_to.tags), rows(&hand_built.tags));
        assert_eq!(rows(&through_reply_to.tags), rows(&through_a_reaction.tags));

        // And the position is the wire's, not the caller's: the target is a
        // reply, so the emitted rows name the ROOT as root and the TARGET as
        // reply -- never the target as root.
        let emitted = rows(&through_reply_to.tags);
        assert_eq!(emitted[0][1], root.id.to_hex());
        assert_eq!(emitted[0][3], "root");
        assert_eq!(emitted[1][1], reply.id.to_hex());
        assert_eq!(emitted[1][3], "reply");
    }

    /// Every pointer emits its author row, and it disappears only when
    /// declined. Catches quartz's shipped NIP-22 reply that silently omitted
    /// the parent author `p` -- invisible to the composing app by
    /// construction, because nothing it can see is missing.
    #[test]
    fn every_pointer_emits_its_author_row_unless_declined() {
        let article = signed(30023, vec![Tag::parse(["d", "my-article"]).unwrap()]);
        let with_author = rows(
            &crate::EventBuilder::new(Kind::from(1111))
                .tag(&article)
                .tags,
        );
        assert!(
            with_author
                .iter()
                .any(|row| row[0] == "p" && row[1] == article.pubkey.to_hex()),
            "the companion p row accompanies every pointer"
        );

        let declined = rows(
            &crate::EventBuilder::new(Kind::from(1111))
                .tag(article.without_author())
                .tags,
        );
        assert!(!declined.iter().any(|row| row[0] == "p"));
        // The author stays in the reference row's own slot: that slot is an
        // outbox hint, not a notification.
        assert!(declined
            .iter()
            .any(|row| row[0] == "e" && row.last() == Some(&article.pubkey.to_hex())));
    }

    /// NIP-22's root scope is uppercase and carries no marker in any
    /// position -- the exact mistake NDK shipped and reverted in `31f7e3bc`.
    #[test]
    fn nip22_root_scope_is_uppercase_with_no_marker_slot() {
        let article = signed(30023, vec![Tag::parse(["d", "my-article"]).unwrap()]);
        let comment = reply_to(&article);
        assert_eq!(comment.kind, Kind::from(COMMENT_KIND));

        let emitted = rows(&comment.tags);
        assert!(emitted.iter().any(|row| row[0] == "A"));
        assert!(emitted.iter().any(|row| row[0] == "K" && row[1] == "30023"));
        assert!(emitted.iter().any(|row| row[0] == "P"));
        for row in &emitted {
            assert!(
                !row.contains(&"root".to_string()) && !row.contains(&"reply".to_string()),
                "a NIP-22 row states importance with case, never with a marker: {row:?}"
            );
        }
    }

    /// Carry-forward is per relationship, not global. A reply carries the
    /// parent's `p` rows (NIP-10 says to); a reaction declines them (NIP-25
    /// says not to). Both dedupe identically, and `without_self` drops the
    /// composing account from either.
    #[test]
    fn carry_forward_and_dedup_behave_identically_on_every_path() {
        let mentioned = Keys::generate().public_key();
        let me = Keys::generate().public_key();
        let note = signed(
            1,
            vec![
                Tag::parse(["p", &mentioned.to_hex()]).unwrap(),
                // A duplicate on the wire must not become a duplicate here.
                Tag::parse(["p", &mentioned.to_hex()]).unwrap(),
                Tag::parse(["p", &me.to_hex()]).unwrap(),
            ],
        );

        let replied = rows(&reply_to(&note).tags);
        let p_rows: Vec<&Vec<String>> = replied.iter().filter(|row| row[0] == "p").collect();
        assert_eq!(
            p_rows.len(),
            3,
            "author plus two distinct carried mentions, deduplicated"
        );
        assert_eq!(p_rows[0][1], note.pubkey.to_hex());

        let reaction = rows(
            &crate::EventBuilder::new(Kind::from(7))
                .tag(note.without_carried_mentions())
                .tags,
        );
        let reaction_p: Vec<&Vec<String>> = reaction.iter().filter(|row| row[0] == "p").collect();
        assert_eq!(
            reaction_p.len(),
            1,
            "a reaction notifies the author and nobody else"
        );

        let without_me = rows(
            &crate::EventBuilder::new(Kind::from(1))
                .tag(note.without_self(me))
                .tags,
        );
        assert!(!without_me
            .iter()
            .any(|row| row[0] == "p" && row[1] == me.to_hex()));
    }

    /// Modifiers are additive and order-independent.
    #[test]
    fn modifiers_compose_in_any_order() {
        let note = signed(30023, vec![Tag::parse(["d", "x"]).unwrap()]);
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let one = crate::EventBuilder::new(Kind::from(1111))
            .tag(note.uppercase().from_relay(relay.clone()))
            .tags;
        let other = crate::EventBuilder::new(Kind::from(1111))
            .tag(note.from_relay(relay.clone()).uppercase())
            .tags;
        assert_eq!(rows(&one), rows(&other));
        assert!(rows(&one)
            .iter()
            .any(|row| row.contains(&relay.to_string())));
    }

    /// The raw `Tag` escape hatch is untouched: it still reaches the wire
    /// verbatim, unvalidated and unreordered.
    #[test]
    fn the_raw_tag_escape_hatch_still_passes_anything_through() {
        let built = crate::EventBuilder::new(Kind::from(9))
            .tag(Tag::parse(["h", "anything-at-all"]).unwrap());
        assert_eq!(
            built.tags[0].as_slice(),
            &["h".to_string(), "anything-at-all".to_string()]
        );
    }
}
