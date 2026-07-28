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
//! THIS FILE OWNS THE STATE AND ITS LIFETIME, AND NOTHING ELSE -- the fields,
//! and the `Drop` that shuts the engine down again (#994), because a field
//! that is never released is not a state declaration a reader can check in
//! isolation. `NmpWorld` is one struct with
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
//! - `identity` -- the identity plane: accounts named by pubkey, the write
//!   that named one, and the restart that proves an accepted write's author
//!   was decided once.
//! - `signers` -- the capability plane: which keys this world can sign for,
//!   when they answer, and who was asked. Distinct from `identity` because
//!   a capability may arrive minutes after the identity was frozen, and the
//!   gap between them is what `awaiting-signer.feature` is about.
//! - `group_fixtures` -- the event a scenario hands the group door: an
//!   unsigned draft, or one signed earlier and published unchanged.
//! - `group_surface` -- what the group door DECLARES, and what the NIP-29
//!   ownership gate says about it: the questions no run can answer.
//! - `groups` -- the NIP-29 `Group` door: staging a group identity, reading
//!   through it, publishing through it, and the wire/receipt facts a group
//!   `Then` reads. Its own module for the same reason `watches` is: a group
//!   scenario asks what reached ONE host and what the delivered event
//!   literally was, which needs bookkeeping no feed step wants.
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
mod group_fixtures;
mod group_surface;
mod groups;
mod identity;
mod observe;
mod queries;
mod signers;
mod staging;
mod watches;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nostr::{EventId, Keys, PublicKey};

use nmp::mechanism::runtime::Handle;
use nmp::Engine;
use nmp_local_signer::LocalKeySigner;

use nmp_test_support::relays::{RelayConfig, ScriptedRelay};

use self::observe::{DiagFeed, FeedState, ReceiptState};
use self::signers::SignerGate;
use self::staging::{PendingContactList, PendingNote};

pub use self::budgets::{EVENTUALLY, NEVER, RECONNECT, WIRE_QUIET, WIRE_SETTLE};
pub use self::group_surface::GroupSurface;
pub use self::groups::{parse_kind_list, GroupCall};
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
    pubkey: PublicKey,
    asked: Arc<AtomicUsize>,
    /// The same ask, attributed to the KEY it was asked of. "Neither A nor B
    /// was asked to sign it" is a claim about which signer was approached,
    /// and the total above cannot answer it.
    asked_by: Arc<Mutex<BTreeMap<PublicKey, usize>>>,
    /// `Given signing fails for this account`. A REFUSAL, not an outage:
    /// `SignerError::Rejected` is terminal for the accepted write, which is
    /// what makes "the failure is reported as a signing failure, not as a
    /// routing failure" a distinguishable claim.
    fails: Arc<AtomicBool>,
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
        NmpWorld::count_ask(&self.asked_by, self.pubkey);
        if self.fails.load(Ordering::SeqCst) {
            return nmp_signer::SignerOp::err(nmp_signer::SignerError::Rejected(
                "nmp-bdd: signing is configured to fail for this account".to_string(),
            ));
        }
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
    /// The same, per key -- see [`CountingSigner::asked_by`].
    signer_asked_by: Arc<Mutex<BTreeMap<PublicKey, usize>>>,
    /// Whether every registered signer refuses (`Given signing fails ...`).
    signer_fails: Arc<AtomicBool>,
    /// Groups I administer, staged as kind:39001 fixtures at the watched
    /// relay (see [`NmpWorld::stage_administered_groups`]).
    pending_groups: Vec<String>,
    group_counter: usize,

    active_person: Option<String>,
    ts_counter: u64,
    switch_counter: u64,
    started: bool,

    // ---- the identity plane (`world::identity`) ----------------------
    /// Every account an identity scenario registered, in the order it did.
    identity_labels: Vec<String>,
    /// Of those, the ones a signing capability is attached for. The
    /// difference between the two lists is the whole subject of
    /// `features/identity/awaiting-signer.feature`.
    identities_with_signers: Vec<String>,
    /// The one a scenario refers back to as "the podcast identity".
    podcast_identity: Option<String>,
    /// Signers the scenario said are SLOW to answer -- registered, asked,
    /// and outstanding until a later step releases them.
    slow_signers: Vec<String>,
    signer_gates: HashMap<String, Arc<SignerGate>>,
    /// One receipt per published text, because an identity scenario may
    /// publish twice and then ask about each by what it said.
    receipts_by_text: HashMap<String, ReceiptState>,
    last_receipt_text: Option<String>,
    /// Which registered identity the last publish RESOLVED to -- what
    /// `Active` meant at the moment it was accepted, not what it would
    /// mean now.
    last_publish_label: Option<String>,
    /// The stable id the publish door returned, for cancel and reattach.
    last_receipt_id: Option<nmp::mechanism::core::ReceiptId>,
    /// The frozen body's id as it stood before a restart, so the far side
    /// can be compared against it byte for byte.
    last_receipt_body: Option<nostr::EventId>,
    /// The reattached stream on the far side of a restart -- the only
    /// stream that exists there.
    restarted_receipt: Option<ReceiptState>,
    /// The display form a user pasted, and what the app decoded it to.
    pasted_npub: Option<String>,
    decoded_identity: Option<PublicKey>,
    /// The app's own refusal when a step handed a display form where a key
    /// belongs.
    identity_refusal: Option<String>,
    /// Whether this scenario runs on a store that outlives its engine.
    /// Set by the identity `Given`s; see `staging::open_store` for why that
    /// is the question, and why it has to be answered before start-up.
    durable_store: bool,
    /// Where that store lives, when there is one. A real store on real disk,
    /// kept for the lifetime of the scenario, is what makes "I reconstruct
    /// the engine from the same durable store" a genuine process boundary
    /// rather than a handle swap.
    store_dir: Option<tempfile::TempDir>,
    store_path: Option<PathBuf>,

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

    engine: Option<Engine>,
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

    // ---- NIP-29 groups (features/groups/) -------------------------------
    /// Staged group identities: group id -> the relay name hosting it. The
    /// `nip29::Group` VALUE cannot exist yet, because a scripted relay has no
    /// URL until it is bound.
    group_hosts: BTreeMap<String, String>,
    /// One `nip29::Group` per id, built on first use and NEVER rebuilt: this
    /// map's insert count is what "no group had to be reconstructed" reads.
    group_values: BTreeMap<String, nmp::nip29::Group>,
    group_builds: BTreeMap<String, usize>,
    /// The group an unqualified "through the group" means: the first staged.
    default_group: Option<String>,
    /// App-supplied read selections, in the order the scenario named them.
    staged_filters: Vec<nmp_grammar::Filter>,
    /// The unsigned draft a scenario staged, kept verbatim so a `Then` can
    /// compare the delivered event against what was actually supplied.
    staged_draft: Option<nmp_grammar::EventBuilder>,
    /// The already-signed event a scenario staged, and the parts it is built
    /// from while the scenario is still adding tags to it.
    staged_signed_parts: Option<group_fixtures::PendingSignedEvent>,
    staged_signed_event: Option<nostr::Event>,
    /// Scenario-visible id LABELS bound to the real ids they stand for. A
    /// `.feature` cannot spell a real event id -- one is only known after
    /// signing -- so `has id "9f2c..."` BINDS that word to the id the event
    /// actually got, and every later step naming it compares against the
    /// binding. What the scenario asserts is identity preservation, which is
    /// exactly what a binding proves.
    id_labels: BTreeMap<String, EventId>,
    /// The typed refusal the last group publication produced, if any.
    group_refusal: Option<nmp::nip29::GroupContextError>,
    /// What the STEP itself named on the last group call.
    group_call: GroupCall,
    /// Relays that are bound (so they have a URL) and then severed, so a
    /// connection to them is refused: `Given relay "R" cannot connect`.
    unreachable_relays: BTreeSet<String>,
    /// What `scripts/check-nip29-ownership.sh` said, and whether it passed.
    gate_outcome: Option<(bool, String)>,
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

impl Drop for NmpWorld {
    fn drop(&mut self) {
        // #994: EngineThread intentionally has no implicit Drop shutdown:
        // production owners must make their lifecycle explicit. A cucumber
        // World is one such owner. Dropping only its JoinHandles detached the
        // engine, pool, transport, verifier, and adapter threads after every
        // scenario, so a full BDD run accumulated one complete engine graph
        // per scenario until the host exhausted memory and threads.
        if let Some(handle) = self.handle.take() {
            handle.shutdown();
        }
        if let Some(engine) = self.engine.take() {
            engine.join();
        }
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
            pubkey: keys.public_key(),
            asked: Arc::clone(&self.signer_asked),
            asked_by: Arc::clone(&self.signer_asked_by),
            fails: Arc::clone(&self.signer_fails),
        }
    }
}
