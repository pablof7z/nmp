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
//! - `contacts` -- the OTHER witness: the scripted relay's own log of what
//!   reached its socket, and what changed since a marked moment. Apart from
//!   `observe` deliberately -- a "never contacted" claim must not rest solely
//!   on the engine's own self-report, or a diagnostics bug could make the
//!   claim un-falsifiable.
//! - `outbox` -- the world an `Auto` write's DEFAULT route is derived from:
//!   the two operator relay sets, the two halves of one person's relay list,
//!   and the three-valued knowledge those scenarios turn on. Distinct from
//!   `staging` because what it stages is what the engine has been able to
//!   LEARN, and because the operator sets belong to nobody in particular.
//! - `routes` -- the other end of that scenario: what the receipt said about
//!   where the write goes, and what it said when the answer was nothing.
//!   Apart from `outbox` because a derivation's inputs and its answer are
//!   separate concerns, and a reader chasing a wrong route wants one or the
//!   other, never both at once.
//! - `staging` -- `Given`-time staging (plain data, no I/O) and the single
//!   lazy `ensure_started` that turns all of it into a running world.
//! - `actions` -- `When`-time acts: open a feed, publish, switch account,
//!   another user posts, a relay drops or comes back.
//! - `facts` -- immediate name-to-fixture lookups. These are staged-world
//!   facts rather than folded runtime observations, so they do not belong in
//!   `observe`.
//! - `identity` -- the identity plane: accounts named by pubkey, and the
//!   write that named one.
//! - `restart` -- the process boundary: stopping the engine and rebuilding
//!   it over the same durable store. Its own module because a restart is a
//!   claim about what SURVIVES rather than about identity, and several
//!   feature directories make one.
//! - `clock` -- what time this world's engine is running at. Its own module
//!   because the stated instant is chosen before the engine exists and has
//!   to be re-applied to the one a restart builds.
//! - `writes` -- the two payload shapes an app hands the publish door: a
//!   builder that carries no author, and an already-signed event that states
//!   one in its own bytes.
//! - `replaceable` -- which version of a whole-value event the store holds,
//!   and what a compare-and-swap replacement did about it. Its own module
//!   because its subject is a third party to every other write plane: the
//!   store's row, which existed before the write did.
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
//! - `stalled` -- the global "is anything quietly stuck" read-out: the
//!   diagnostics section that describes obligations nobody holds a receipt
//!   for, and the two acts a scenario about it performs (publishing to a
//!   destination this world deliberately never starts, and reading the list
//!   repeatedly to prove that reading is not part of what it describes).
//! - `provenance` -- WHICH RELAYS served a row the app is looking at, and the
//!   host-signed fixtures that make two relays disagree about one addressable
//!   coordinate. Its own module because every other module here is about what
//!   a row IS or how it got asked for; this one is about who delivered it,
//!   which is a fact carried alongside every row and asserted by none of them.
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
mod clock;
mod contacts;
mod facts;
mod group_fixtures;
mod group_surface;
mod groups;
mod identity;
#[cfg(test)]
mod identity_tests;
mod observe;
mod outbox;
mod provenance;
mod queries;
mod replaceable;
mod restart;
mod routes;
mod signers;
mod staging;
mod stalled;
mod watches;
mod writes;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nostr::{EventId, Keys, PublicKey, Timestamp};

use nmp::mechanism::runtime::Handle;
use nmp::Engine;
use nmp_local_signer::LocalKeySigner;

use nmp_test_support::relays::{RelayConfig, ScriptedRelay};

use self::observe::{DiagFeed, FeedState, ReceiptState};
use self::signers::SignerGate;
use self::staging::PendingContactList;
use self::writes::ComposedWrite;

pub use self::budgets::{EVENTUALLY, NEVER, RECONNECT, WIRE_QUIET, WIRE_SETTLE};
pub use self::clock::{format_stated_time, parse_stated_time};
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
    /// The INBOX half of a person's relay list -- what an outbox fan-out
    /// reaches a p-tagged recipient at. Deliberately separate from
    /// `write_relay_of`: a recipient is reached at their read relays and
    /// never at their write set, and a scenario that confused the two would
    /// pass for the wrong reason.
    read_relay_of: HashMap<String, Vec<String>>,
    /// People whose relay list is staged as REAL and EMPTY -- a kind:10002
    /// that declares no relays at all. "Known, zero relays" is a fact; it is
    /// not the same as never having published one, and the whole point of
    /// three-valued knowledge is that those two do not collapse.
    declares_no_relays: Vec<String>,
    /// People whose relay list EXISTS and names no WRITE relay -- the
    /// half-empty case, which turns up in the wild as a list whose entries
    /// are all read-marked. Their read half may still name relays, so this
    /// cannot be folded into `declares_no_relays`.
    declares_no_write_relays: Vec<String>,
    /// The operator's two additive relay sets. Neither belongs to any
    /// person, which is why they are named here rather than in
    /// `write_relay_of`/`read_relay_of`: `app_relays` reaches every kind of
    /// every author always, and `fallback_relays` tops up a p-tagged
    /// recipient below the coverage minimum unless an app relay suppressed
    /// it.
    app_relay_names: Vec<String>,
    fallback_relay_names: Vec<String>,
    /// Indexers currently withholding their end-of-stored-events. One
    /// unfinished source is enough to keep every absence unsettled, and
    /// keeping the OTHER indexer well-behaved is what lets a relay list
    /// still arrive while nothing settles.
    withholding_indexers: Vec<String>,
    /// When the last publish went out, on the same wall clock the engine
    /// stamps a stalled write with -- the lower bound that makes
    /// "how long it has been stuck" a recorded fact rather than a number.
    last_publish_at: Option<nostr::Timestamp>,

    pending_contact_lists: Vec<PendingContactList>,
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
    /// Kind-39000 group metadata staged for a NAMED HOST to sign
    /// (relay, group id, name), seeded once every relay is bound. See
    /// [`world::provenance`](self::provenance) for why the host, not a
    /// member, is the signer.
    pending_group_metadata: Vec<(String, String, String)>,

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

    // ---- the clock plane (`world::clock`) ------------------------------
    /// The instant this scenario stated its device clock reads, if any.
    /// Owned here rather than on the engine because it is chosen BEFORE the
    /// engine exists and has to be re-applied to the fresh engine a restart
    /// builds -- the same lifetime `durable_store` has, for the same reason.
    pinned_clock: Option<Timestamp>,

    // ---- the write plane (`world::writes`) -----------------------------
    /// The builder a `When I compose ...` staged and has not published yet,
    /// with the text it says. Two steps rather than one because a scenario
    /// that hands over a tag table composes on one line and publishes on the
    /// next.
    pending_builder: Option<nmp_grammar::EventBuilder>,
    /// Every builder this scenario published, in order, each pointing at the
    /// entry it took in [`Self::receipts`]. Only the BUILDERS are kept here;
    /// their receipts live in the world's one ordered publish list (#995),
    /// which is also why this is a list at all -- `event-builder.feature`
    /// publishes the SAME text twice and then asks about each, so the
    /// identity plane's by-text map cannot tell them apart.
    composed: Vec<ComposedWrite>,
    /// Whole signed events bound to the id word a scenario names them by.
    /// Distinct from [`Self::id_labels`], which binds a word to an id: a
    /// pre-signed scenario publishes the EVENT, and then asks whether the
    /// bytes that arrived are the bytes it handed over.
    signed_by_label: BTreeMap<String, nostr::Event>,
    /// The event a `When I publish ... as-is` actually handed the door,
    /// including a deliberately corrupted one.
    handed_over: Option<nostr::Event>,

    // ---- the replaceable plane (`world::replaceable`) -------------------
    /// A replacement composed against a base and not yet published, so a
    /// scenario can move the winner underneath it first. The gap between
    /// composing and publishing is what makes "checked at acceptance, not at
    /// compose time" a claim with two distinguishable answers.
    pending_replacement: Option<(
        nmp_grammar::Identity,
        Option<EventId>,
        nmp_grammar::EventBuilder,
    )>,
    /// Contact lists by an author this world cannot sign for, which reached
    /// the store the only way a foreign event ever does -- observed from a
    /// relay -- keyed by whose they are.
    foreign_contact_lists: BTreeMap<String, String>,
    /// What the scenario SAID each staged version's timestamp was, keyed by
    /// the word it named it with. Held rather than read back off the wire
    /// because #995 retires a displaced predecessor's outbox obligation, so a
    /// version that is legitimately the store's may correctly never reach any
    /// relay.
    stated_created_at: BTreeMap<String, Timestamp>,

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
    /// Every publish this scenario made, in the order it made them. Most
    /// scenarios publish once and only ever ask about the last; a scenario
    /// about one write RETIRING another needs both obligations at the same
    /// time, so the world keeps the whole list rather than the newest.
    receipts: Vec<ReceiptState>,
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

    // ---- stalled writes (`world::stalled`) ------------------------------
    /// The literal relay URLs a scenario TOLD this world to publish to. Kept
    /// as URLs rather than relay names because the case they exist for is a
    /// destination this world deliberately never starts.
    told_route: Vec<nmp_router::RelayUrl>,
    /// The snapshot the last `I read diagnostics` returned -- "the list"
    /// every following assertion reads.
    last_diagnostics: Option<nmp::mechanism::core::DiagnosticsSnapshot>,
    /// One fingerprint per read of a repeated read, so "reading changed
    /// nothing" compares every answer instead of only the last.
    repeated_diagnostics: Vec<Vec<(String, String, u64)>>,
    /// The descriptor of the row a scenario named, so a later step can prove
    /// THAT row left rather than merely that the list shrank.
    named_stalled_write: Option<String>,
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
        // #977 moved the world onto `nmp::Engine`, whose `shutdown` is what
        // asks the thread to stop and then joins it. `staging::stop_engine`
        // is the single definition of that sequence -- dropping the cloned
        // handle before anything blocks -- shared with the identity
        // scenarios' restart so teardown and restart cannot drift apart.
        self.stop_engine();
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

    /// The active account's pubkey in the hex spelling a park's reason uses.
    /// Internal identity is `PublicKey`/hex everywhere; bech32 is outward
    /// decoration only and never appears here.
    pub fn my_pubkey_hex(&mut self) -> String {
        let me = self
            .active_person
            .clone()
            .expect("nmp-bdd: 'me' needs a logged-in account");
        self.person(&me).public_key().to_hex()
    }

    /// Every relay named as `person`'s INBOX -- what an outbox fan-out would
    /// reach them at, and therefore exactly the set a "nothing was contacted
    /// on their behalf" assertion has to look at.
    pub fn read_relay_names_of(&self, person: &str) -> Vec<String> {
        self.read_relay_of.get(person).cloned().unwrap_or_default()
    }

    /// The OUTBOX half: every relay `person` declared as their own write
    /// relay.
    pub fn write_relay_names_of(&self, person: &str) -> Vec<String> {
        self.write_relay_of.get(person).cloned().unwrap_or_default()
    }

    /// The name of the person this world calls "me".
    pub fn me(&self) -> String {
        self.active_person
            .clone()
            .expect("nmp-bdd: 'me' needs a logged-in account")
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
