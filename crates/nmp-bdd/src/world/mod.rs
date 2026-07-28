//! `NmpWorld` — one fresh world per scenario (approach doc §2.2): fresh
//! engine, fresh in-process relays, spawned lazily so every `Given` (relay
//! topology, seeded protocol state, operator config) is staged as plain data
//! before anything hits a real socket or the real `EngineThread`. The first
//! step that actually needs the engine calls
//! [`ensure_started`](NmpWorld::ensure_started), which starts every staged
//! `ScriptedRelay`, seeds their fixture events, and spawns the real
//! `nmp::mechanism::runtime::EngineThread` against them -- never a mocked
//! engine, never a resolver-only shortcut (§2.1).
//!
//! Everything under this module is internal plumbing for the step catalog
//! (`steps::{given,when,then}`); scenarios never see it directly, and the
//! four observables it exposes (`feed_*`, `receipt_*`, `diagnostics_*`,
//! `relay_contacted`/`relay_contact_count`) are the ONLY things a `Then`
//! step is allowed to assert on (approach doc §1.3).
//!
//! THIS FILE OWNS THE STATE AND NOTHING ELSE. `NmpWorld` is one struct with
//! one lifetime (the scenario), so its fields have to be declared in one
//! place to be read as a whole; its BEHAVIOUR splits cleanly by the phase of
//! a scenario it serves, and each of those phases is a sibling module below.
//! Rust's module privacy makes that split free: the fields stay private to
//! `world`, and every child module can still reach them, so the boundary
//! costs no accessors and leaks nothing to `steps`.
//!
//! - `budgets` -- every bounded-wait duration in the suite, and why each one
//!   is its own number rather than a reuse of a neighbour.
//! - `queries` -- the fixture `LiveQuery` catalog: the shapes a scenario
//!   names ("my follows' notes", "notes tagged p as alice").
//! - `observe` -- the observation plane: the accumulating channels
//!   (`FeedState`/`ReceiptState`/`DiagFeed`) and the bounded observers a
//!   `Then` step reads them through.
//! - `staging` -- `Given`-time staging (plain data, no I/O) and the single
//!   lazy `ensure_started` that turns all of it into a running world.
//! - `actions` -- `When`-time acts: open a feed, publish, switch account,
//!   another user posts, a relay drops or comes back.
//! - `watches` -- watching one named relay directly, which is a separate
//!   concern from the feed: it exists to observe what NMP puts on a SOCKET,
//!   so it owns the watch bookkeeping, the group fixtures that feed it, and
//!   the wire records assertions read.
//!
//! The submodules are PRIVATE and everything they export is re-exported
//! here, so `nmp_bdd::world::*` names exactly what it named before the split
//! -- a step file never has to know which of them a helper ended up in.

mod actions;
mod budgets;
mod observe;
mod queries;
mod staging;
mod watches;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use nostr::Keys;

use nmp::mechanism::runtime::{EngineThread, Handle};
use nmp_local_signer::LocalKeySigner;

use nmp_test_support::relays::{RelayConfig, ScriptedRelay};

use self::observe::{DiagFeed, FeedState, ReceiptState};
use self::staging::{PendingContactList, PendingNote};

pub use self::budgets::{EVENTUALLY, NEVER, RECONNECT, WIRE_QUIET, WIRE_SETTLE};
pub use self::queries::{
    authored_note_query, my_follows_query, my_group_state_query, tagged_note_query, WatchShape,
};

/// The canonical name for the scenario's own (implicit "I"/"my") account --
/// every `my`/`I`-phrased step resolves through this one name, so "my
/// account" always names the same keypair as "I".
pub const ME: &str = "me";

/// #765: `LocalKeySigner` now owns its scalar in one canonical zeroizing
/// allocation and no longer accepts a `nostr::Keys`. These fixtures still
/// build identities as `Keys`, so hand the raw scalar across exactly here.
fn local_signer(keys: &Keys) -> LocalKeySigner {
    LocalKeySigner::from_secret_bytes(keys.secret_key().as_secret_bytes())
        .expect("fixture keys are valid secp256k1 scalars")
}

/// A real signer that also counts how many times it was ASKED to sign.
///
/// "No signer was asked for anything" is otherwise unobservable from the
/// receipt stream: `WriteStatus::Signed` is a lifecycle beat the engine
/// emits for an already-signed payload too, so reading it would make the
/// assertion mean something other than what it says. Counting the actual
/// capability call is the fact, and it stays a fact whether the write was
/// refused at the door or carried its own signature.
struct CountingSigner {
    inner: LocalKeySigner,
    asked: Arc<AtomicUsize>,
}

impl nmp_signer::SigningCapability for CountingSigner {
    fn public_key(&self) -> Option<nmp_signer::SignerPublicKey> {
        self.inner.public_key()
    }

    fn is_available(&self) -> bool {
        self.inner.is_available()
    }

    fn sign(
        &self,
        unsigned: nmp_signer::SignerUnsignedEvent,
    ) -> nmp_signer::SignerOp<nmp_signer::SignerSignedEvent> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.inner.sign(unsigned)
    }
}

#[derive(cucumber::World, Default)]
pub struct NmpWorld {
    people: HashMap<String, Keys>,

    relay_configs: HashMap<String, RelayConfig>,
    relay_order: Vec<String>,
    relays: HashMap<String, ScriptedRelay>,
    indexer_names: Vec<String>,
    write_relay_of: HashMap<String, Vec<String>>,

    pending_contact_lists: Vec<PendingContactList>,
    pending_notes: Vec<PendingNote>,
    /// Notes staged as already-signed events, kept verbatim so a later step
    /// can republish exactly what their author signed.
    pending_signed_notes: Vec<(String, nostr::Event)>,
    signed_notes: HashMap<String, nostr::Event>,
    /// The already-signed event the last republish step handed over.
    republished: Option<nostr::Event>,
    /// Whether the last publish said "figure it out" rather than naming
    /// relays -- what "the app named no relay anywhere" reads.
    last_publish_was_auto: bool,
    /// How many times a registered signer was asked to sign anything.
    signer_asked: Arc<AtomicUsize>,
    /// Groups I administer, staged as kind:39001 fixtures at the watched
    /// relay (see [`NmpWorld::stage_administered_groups`]).
    pending_groups: Vec<String>,
    group_counter: usize,

    active_person: Option<String>,
    ts_counter: u64,
    switch_counter: u64,
    started: bool,

    /// The relay every `watch` step pins its demand to -- the subject of the
    /// subscription-collapse scenarios.
    watch_relay: Option<String>,
    /// Open watches, keyed by the scenario-visible thing being watched
    /// (`"p=alice"`, `"author=Alice"`), so one of N can be closed by name.
    watches: BTreeMap<String, FeedState>,
    /// Which values of which single-letter tag are watched RIGHT NOW (a
    /// closed watch is removed) -- what "every value I watch" resolves to.
    watched_tag_values: BTreeMap<char, BTreeSet<String>>,
    /// The same, for the author-axis control.
    watched_authors: BTreeSet<String>,

    engine: Option<EngineThread>,
    handle: Option<Handle>,
    feed: Option<FeedState>,
    last_receipt: Option<ReceiptState>,
    diag: Option<DiagFeed>,
    contact_snapshot: HashMap<String, u64>,
    /// Taken alongside [`Self::contact_snapshot`]: how many REQ/CLOSE frames
    /// each relay's wire log already held at that moment, so an "untouched"
    /// failure can name the frames that arrived AFTER the snapshot instead
    /// of just reporting that a counter moved.
    wire_snapshot: HashMap<String, (usize, usize)>,
    /// Also taken alongside it: how many client connections each relay had
    /// accepted. A REQ that arrives together with a NEW connection is a
    /// reconnect replay, not a recompile.
    connection_snapshot: HashMap<String, u64>,
    /// And how many EVENTs each relay had already admitted, so an
    /// unexplained contact-count move can name the KINDS that landed.
    admitted_snapshot: HashMap<String, usize>,
}

impl std::fmt::Debug for NmpWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NmpWorld")
            .field("people", &self.people.keys().collect::<Vec<_>>())
            .field("relays", &self.relay_order)
            .field("indexers", &self.indexer_names)
            .field("active_person", &self.active_person)
            .field("started", &self.started)
            .field("feed_open", &self.feed.is_some())
            .finish()
    }
}

impl NmpWorld {
    /// The started engine's handle -- every `When` action and every watch
    /// goes through here, so "the engine must be started first" is stated
    /// once instead of at each call site.
    pub(crate) fn handle(&self) -> &Handle {
        self.handle
            .as_ref()
            .expect("nmp-bdd: the engine must be started (ensure_started) before use")
    }

    /// Wrap `keys` in the counting signer, sharing this world's one counter.
    /// Every `add_signer` call goes through here so `signer_ask_count` reads
    /// every signer the scenario registered, not just the first.
    pub(super) fn counting_signer(
        &self,
        keys: &Keys,
    ) -> impl nmp_signer::SigningCapability + use<> {
        CountingSigner {
            inner: local_signer(keys),
            asked: Arc::clone(&self.signer_asked),
        }
    }
}
