//! `nmp-store` — `EventStore` trait + `MemoryStore` + `RedbStore`: the one
//! mutating door (VISION §4 "the store", bug-class ledger #1), extended in
//! M3 step A1 with persistence, provenance merge, and coverage watermarks
//! (VISION §7 ledger #7 / #5).
//!
//! Insert runs **dedup-by-id first**, THEN replaceable/addressable
//! supersession (M1 plan §2.2): winner = newest `created_at`, tie-break
//! lexicographically-smallest id. `query` reuses `nostr::Filter::match_event`
//! — no hand-rolled event matching. A duplicate-id insert now MERGES relay
//! provenance into the stored row (ledger #5) instead of being a no-op.
//!
//! Coverage (`record_coverage`/`get_coverage`) implements the store half of
//! `docs/design/query-demand-and-evidence.md` and issue #816's
//! facts-before-claims contract — see [`coverage`] for the full recap.
//! Claim-based bounded GC (`gc`) evicts only regular (non-addressed) events
//! matched by no live claim, lowering any coverage row it invalidates in the
//! same step.
//!
//! Retraction (`docs/design/retraction-and-negative-deltas.md`, issue #28):
//! kind:5 (NIP-09) deletion runs inside `insert` and writes PERMANENT
//! tombstones (§7 owner decision — never GC-claimed) so a later redelivery
//! of a deleted event is `Refused(Tombstoned)`; NIP-40 `expiration` is
//! tracked in a persistent index so `expire_due`/`next_expiration` are
//! index-backed, not O(stored rows).
//!
//! Durable write-delivery (`docs/design/crashsafe-accepted-2-3-plan.md`,
//! issues #2/#3, Fable checkpoint verdict Q2): this crate is now the event
//! **and** publish-queue store in the current Redb implementation — one
//! atomic `redb::Database` boundary. This is an implementation shape, not a
//! requirement that every backend or platform use one physical engine. A
//! split implementation must keep each authority internally atomic, persist
//! control intent before event projection, replay deterministically and
//! idempotently, and reconcile before serving queries or transport. A
//! locally-authored write intent enters through [`EventStore::accept_write`]
//! (the same dedup/tombstone/supersession rules `insert` runs, stamping
//! local provenance + [`SigState::Pending`] instead of a `RelayObserved`),
//! committing the pending row AND the durable intent/displaced-stash journal
//! in ONE transaction. [`EventStore::promote_signed`] swaps the real
//! signature in place (zero id churn — a NIP-01 id never depends on `sig`)
//! and durably drops the displaced stash. [`EventStore::compensate_write`]
//! undoes a pre-signature-terminated intent: `remove(id, Rejected)` (no
//! tombstone — the row was never validly signed) plus a compensating
//! re-`insert` of whatever it displaced, through the same one door.
//! [`EventStore::recover_publish_queue`] replays every still-open intent after a
//! restart. Exact resolved relay sets use a separate append-only route-
//! revision door which commits before any corresponding attempt. Every policy
//! decision (retry ownership, deadline scheduling, signer orchestration) stays
//! in `nmp-engine`; the store exposes only typed doors — never raw table/
//! transaction access.
//!
//! Two architecture-review corrections load-bear on the above: (1)
//! [`IntentId`] is allocated by the STORE from a durable high-water mark
//! bumped inside `accept_write`'s own transaction — never caller-supplied
//! (see its doc for the reuse hazard this closes); (2) receipt identity/
//! state is retained under `PUBLISH_QUEUE_RECEIPTS`, independently of
//! `PUBLISH_QUEUE_INTENTS`'s open-work row, so [`EventStore::reattach_receipt`]
//! keeps answering for a terminal receipt after its open-work row is gone
//! (see [`ReceiptState`]'s doc).
//!
//! No store implementation verifies a signature: the one
//! `nostr::Event::verify` call an accepted signer result must pass happens
//! on the caller's side, in `nmp-engine`. What it produces is a
//! [`VerifiedSignature`], the only value `promote_signed` accepts — so the
//! precondition is carried by a type instead of asserted in prose (#768),
//! and the door still binds it to the intent's own frozen id before
//! mutating anything. The engine's send-time attribution snapshots stay out
//! of scope too (this crate only stores whatever interval it is told to
//! record).

mod address_key;
mod binary_event;
mod coverage;
mod coverage_claims;
mod memory_store;
mod persistent_store_lifetime;
mod redb_store;
#[cfg(test)]
mod semantic_oracle;

#[cfg(feature = "bench-instrumentation")]
pub mod ingest_attribution;

pub use coverage::{coverage_key, CoverageInterval, CoverageKey, GcReport, GcRetentionSet};
pub use coverage_claims::coverage_claim_atoms;
pub use memory_store::MemoryStore;
pub use persistent_store_lifetime::{RedbStoreOpenError, RedbStoreResetError};
pub use redb_store::RedbStore;
#[cfg(feature = "bench-instrumentation")]
pub use redb_store::{
    prepare_equivalent_store_corpus, run_fjall_governed_ingest_bench,
    run_lmdb_governed_ingest_bench, run_packed_postings_bench,
    run_prepared_redb_compact_index_bench, run_prepared_redb_redo_index_bench,
    run_prepared_redb_store_bench, run_prepared_redb_unified_index_bench, run_store_bench_variant,
    FjallGovernedIngestMetrics, LmdbGovernedIngestMetrics, LmdbPackedWork, PackedPostingsBackend,
    PackedPostingsMetrics, PackedQueryMetrics, RedbRedoIndexMetrics, StoreBenchAttribution,
    StoreBenchMetrics, StoreBenchPreparedBatch, StoreBenchPreparedCorpus,
    StoreBenchPreparedMetrics, StoreBenchPreparedRecord, StoreBenchPreparedTable,
    StoreBenchProcessCounters, StoreBenchVariant,
};

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::ContextualAtom;
use nostr::secp256k1::schnorr::Signature;
use nostr::{Event, EventId, Filter, PublicKey, RelayUrl, Timestamp};
use serde::{Deserialize, Serialize};

/// Stable identifier for a durable write intent, ALLOCATED BY THE STORE
/// ITSELF from a durable, monotonically-advancing high-water mark
/// (`PUBLISH_QUEUE_META` for `RedbStore`) bumped inside the SAME `accept_write`
/// transaction that journals the intent — never inferred from the
/// currently-open set.
///
/// This is a load-bearing correction (architecture review, post-initial-
/// build): an earlier revision of this door took a CALLER-assigned
/// `IntentId` and left allocation to `nmp-engine`. That is unsound the
/// moment R8-style terminal cleanup exists: `PUBLISH_QUEUE_INTENTS` rows are
/// deleted once an intent's open work concludes (`compensate_write` today;
/// a future all-lanes-terminal path later), so a caller-side allocator that
/// infers "next free" from the currently-*open* recovered set will
/// eventually reissue an id that a terminated intent already used —
/// colliding with that intent's still-*retained* [`PublishQueueReceipt`] (see
/// [`EventStore::reattach_receipt`]) or any retained per-relay attempt
/// evidence. Issue #3's "ids remain stable and unique across restart"
/// means unique for the store's ENTIRE lifetime, not merely among what
/// recovery currently sees open — so allocation must be a fact the store
/// itself owns and persists, never a value trusted in from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IntentId(pub u64);

/// Signature state of a locally-authored row, as data on the row itself
/// (`docs/design/retraction-and-negative-deltas.md` §4.1 — "not a second
/// query path or committed/pending authority split"). Exposed on
/// [`LocalOrigin`] so the app surface can always tell a sentinel-sig
/// pending row from a really-signed one (Fable checkpoint Q1 condition a).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SigState {
    /// The row's `sig` is [`sentinel_signature`] — not yet signed.
    Pending,
    /// The row carries a real, caller-verified signature.
    Signed,
}

/// A locally-authored row's provenance (issue #2's "`Local` origin; a row
/// *field*, exactly ledger #5's shape"). Set iff this row entered through
/// [`EventStore::accept_write`] rather than [`EventStore::insert`].
///
/// `owners` is a SET, not a single `IntentId` (architecture review
/// correction, team-lead decision on issue #2): an earlier revision
/// conflated "this row's canonical signature state" with "the one intent
/// that backs it," which broke the moment a byte-identical `Duplicate`
/// intent was accepted against an already-locally-owned row — cancelling
/// the FIRST intent would remove the row out from under a SECOND intent
/// still durably obligated to deliver it (its own `PUBLISH_QUEUE_INTENTS`/receipt
/// stayed open with no canonical row to promote or compensate). Every
/// accepted intent that currently backs this row's existence is a member;
/// coalescing duplicates into one owner was rejected because it would
/// silently drop a later intent's own receipt, violating "every accepted
/// write returns a receipt." `sig_state` stays canonical to the ROW, never
/// per-owner: ANY owner's [`EventStore::promote_signed`] call sets it, in
/// place, for every owner at once — there is exactly one signature on one
/// row, however many intents are backing it.
///
/// [`EventStore::compensate_write`] on one owner only removes THAT owner
/// from the set; the canonical row is only actually retracted once the set
/// is empty AND `sig_state` is still `Pending` AND no relay has
/// independently confirmed it (`Provenance::seen` empty) — an owner-less
/// row that is already `Signed`, or that a relay has confirmed on its own,
/// is left standing with an empty `owners` set rather than deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalOrigin {
    pub owners: BTreeSet<IntentId>,
    pub sig_state: SigState,
}

/// Per-relay provenance for one stored event: which relays have delivered
/// this exact event id, and the latest wall-clock time each one did so
/// (ledger #5). A first-class field of the stored row, not a sidecar.
/// `local` is `Some` iff this row has ever been locally accepted (issue
/// #2) — it is preserved (never cleared) across a later relay echo merging
/// into `seen`, AND across every owning intent eventually being
/// compensated away (`LocalOrigin::owners` can be empty while `local`
/// stays `Some`, e.g. once relay provenance alone sustains the row — see
/// [`LocalOrigin`]'s doc): the app's "sending…" chip resolves off
/// `seen.is_empty()`, not off `local`'s presence (retraction doc §4.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    pub seen: BTreeMap<RelayUrl, Timestamp>,
    pub local: Option<LocalOrigin>,
}

impl Provenance {
    /// A fresh `Provenance` recording exactly one observation.
    pub(crate) fn first_observation(from: RelayObserved) -> Self {
        let mut seen = BTreeMap::new();
        seen.insert(from.relay, from.at);
        Self { seen, local: None }
    }

    /// A fresh `Provenance` for a row entering through `accept_write`: no
    /// relay has observed it yet, but it carries local provenance.
    pub(crate) fn local_origin(local: LocalOrigin) -> Self {
        Self {
            seen: BTreeMap::new(),
            local: Some(local),
        }
    }

    /// Merge one more observation in. Returns `true` iff this observation
    /// changed the map: a relay not seen before, or a strictly later
    /// timestamp for a relay already seen. A redelivery from a relay at an
    /// equal-or-earlier timestamp than what is already recorded changes
    /// nothing and returns `false` — no index churn on a no-op merge.
    /// Never touches `local` — a relay echo of an already-local row keeps
    /// its local provenance (retraction doc §4.1).
    pub(crate) fn merge_observation(&mut self, from: &RelayObserved) -> bool {
        match self.seen.get(&from.relay) {
            None => {
                self.seen.insert(from.relay.clone(), from.at);
                true
            }
            Some(existing) if *existing < from.at => {
                self.seen.insert(from.relay.clone(), from.at);
                true
            }
            Some(_) => false,
        }
    }

    /// Whether a projection pinned to `pinned` may serve this row.
    ///
    /// Two facts that must not be conflated: whether a row APPEARS in a
    /// projection, and whether a relay CARRIED it.
    ///
    /// A FOREIGN row — one that reached this node because some relay
    /// delivered it — answers only for the relays that delivered it. That is
    /// the cross-host isolation a pinned read exists for: one host's cached
    /// rows never answer for a host that did not serve them.
    ///
    /// OUR OWN row is not that case at all. It entered through
    /// [`EventStore::accept_write`], it is in the outbound publication queue,
    /// and it is ours whatever any relay subsequently does with it.
    /// Withholding it would make every pinned live query lie about what the
    /// user just did, and withdrawing it later would be worse: the feed would
    /// delete the user's own text on the strength of a host it is not even
    /// watching. Its provenance is still reported honestly — the relays that
    /// carried it, which may be none of them, may be all of them, and may be
    /// none of the pinned ones.
    ///
    /// So the distinction is ours versus foreign, spelled `local.is_some()`
    /// — never carried versus uncarried. A row keeps its local origin
    /// forever, including long after relay provenance arrives, which is
    /// exactly the property this needs: publishing to two hosts and watching
    /// one of them must not make the answer depend on the other (#1191).
    /// Empty `seen` remains what it always was, the fact an app's "sending…"
    /// chip resolves off, and it decides nothing here.
    #[must_use]
    pub fn visible_under_pin(&self, pinned: &BTreeSet<RelayUrl>) -> bool {
        visible_under_pin(self.local.is_some(), self.seen.keys(), pinned)
    }
}

/// [`Provenance::visible_under_pin`] for the callers that hold its two
/// inputs without holding a whole [`Provenance`]: a projected committed row,
/// or a persistent backend testing visibility against its index rather than
/// against a decoded row. One rule, one spelling, three call sites.
#[must_use]
pub fn visible_under_pin<'a>(
    ours: bool,
    carried_by: impl IntoIterator<Item = &'a RelayUrl>,
    pinned: &BTreeSet<RelayUrl>,
) -> bool {
    ours || carried_by.into_iter().any(|relay| pinned.contains(relay))
}

/// The sentinel signature every pending row's frozen body carries until
/// [`EventStore::promote_signed`] swaps in the real one (Fable checkpoint
/// Q1, APPROVED): a NIP-01 id is `hash([0,pubkey,created_at,kind,tags,
/// content])` — the signature is not an id input — so an all-zero 64-byte
/// value round-trips through `nostr::Event`/JSON/`Filter::match_event`
/// unverified (schnorr `Signature` parsing is length-checked only) and the
/// id is final before a real signature exists.
pub fn sentinel_signature() -> Signature {
    Signature::from_slice(&[0u8; 64])
        .expect("64 zero bytes is always a structurally valid (length-checked) schnorr signature")
}

/// The only thing [`EventStore::promote_signed`] accepts (#768): a
/// signature that a `nostr::Event::verify` call actually passed, carried
/// together with the [`EventId`] it was verified against.
///
/// `promote_signed` used to take a bare `Signature` plus a doc sentence
/// telling the caller to have verified it first. A sentence is not a guard:
/// any store consumer could hand the door a signature belonging to a
/// different event, or to no event at all, and the production stores would
/// still replace the sentinel, flip every co-owner to `Signed`, drop the
/// displaced recovery stash, and — for a pending kind:5 draft — turn
/// provisional suppression claims into PERMANENT tombstones. That is the
/// convention-only failure class `docs/bug-class-ledger.md:3-5` rules out,
/// and the precondition the Destructive-API Gate requires be typed.
///
/// The fields are private and [`Self::verify`] is the only constructor, so
/// the value cannot exist unless one verification succeeded. `Event::verify`
/// recomputes the NIP-01 id from the body and checks the schnorr signature
/// against THAT id and pubkey, so [`Self::event_id`] is not a label the
/// caller chose — it is the identity the signature actually covers. The
/// store compares it to the intent's own durable frozen id, which is what
/// binds the evidence to *this* write rather than to merely some valid one.
///
/// Verification stays a caller-side act performed exactly once (#387): the
/// engine's signer-result validation constructs this value and hands it
/// down, and no store implementation runs a second Schnorr check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedSignature {
    event_id: EventId,
    signature: Signature,
}

impl VerifiedSignature {
    /// Verify `event` whole — id recomputed from the body, schnorr
    /// signature checked against that id and `event.pubkey` — and keep the
    /// proof. `Err` is `nostr`'s own verification failure, unchanged.
    pub fn verify(event: &Event) -> Result<Self, nostr::event::Error> {
        event.verify()?;
        Ok(Self {
            event_id: event.id,
            signature: event.sig,
        })
    }

    /// The id the signature was verified against. A store door matches this
    /// against the intent's frozen id before it mutates anything.
    #[must_use]
    pub fn event_id(&self) -> EventId {
        self.event_id
    }

    /// The verified signature itself — what actually replaces
    /// [`sentinel_signature`] on the canonical row.
    #[must_use]
    pub fn signature(&self) -> Signature {
        self.signature
    }
}

/// Re-freeze `frozen` at `created_at`, re-deriving its NIP-01 id over the
/// stamped body. Used by the acceptance transaction to apply
/// [`AcceptWrite::monotonic_stamp`] against the row it is CAS-ing; the
/// signature stays [`sentinel_signature`] because the body is still
/// pre-signature at this point (which is precisely why moving the stamp
/// here is possible at all).
pub(crate) fn restamped(frozen: &Event, created_at: Timestamp) -> Event {
    Event::new(
        EventId::new(
            &frozen.pubkey,
            &created_at,
            &frozen.kind,
            &frozen.tags,
            &frozen.content,
        ),
        frozen.pubkey,
        created_at,
        frozen.kind,
        frozen.tags.clone(),
        frozen.content.clone(),
        frozen.sig,
    )
}

/// A durable-persistence failure at the acceptance boundary
/// (`docs/design/durable-write-signing-and-retry.md` §1: "If that
/// transaction fails, the caller receives an acceptance error and no
/// pending row becomes visible" — architecture review correction).
/// Realistic runtime failures (disk full, I/O error) at `accept_write`/
/// `accept_refused`/`promote_signed`/`compensate_write` must never panic
/// the embedding app. Neither may a *persisted row that does not decode*
/// (#790): a malformed, truncated, or schema-incompatible value is a fact
/// about the file, not a reason to abort the host, so every production
/// decoder of store-owned bytes/JSON reports it through its owning door as
/// [`PersistenceFault::Invariant`] instead of `.expect()`ing.
/// `MemoryStore` implements the same fallible signature for backend
/// uniformity but never actually returns `Err` (it does no I/O and owns no
/// encoded rows).
///
/// "Do not panic" was only half the contract (#895). The other half is
/// telling the embedder *what kind* of failure this was, so recovery is a
/// branch on a type rather than a `contains("Previous I/O")` on a string:
/// [`PersistenceError::fault`] carries the backend classification and
/// [`PersistenceError::durability`] carries the durability outcome. The
/// message is preserved verbatim for display.
#[derive(Debug)]
pub struct PersistenceError {
    fault: PersistenceFault,
    message: String,
}

impl PersistenceError {
    /// A failure the store raised about its own contents or arguments, not
    /// a backend I/O failure: a decode/encode failure, a schema invariant,
    /// an index disagreement, an exhausted counter. Nothing durable was
    /// written, and reopening the handle changes nothing.
    ///
    /// This is the constructor for every in-store `Err(...)` that is not a
    /// backend error funneled through `redb_store::schema::persist_err`.
    pub fn invariant(message: impl Into<String>) -> Self {
        Self::new(PersistenceFault::Invariant, message)
    }

    /// Build an explicitly classified failure. [`EventStore`] is a public
    /// trait, so an out-of-crate backend (or a fault-injecting test double)
    /// must be able to report a latch or an indeterminate I/O failure, not
    /// only an invariant.
    ///
    /// Claim the weakest fault that is true. [`PersistenceFault::Latched`]
    /// asserts the operation was never attempted; if that is not provable,
    /// [`PersistenceFault::Io`] is the honest answer.
    pub fn new(fault: PersistenceFault, message: impl Into<String>) -> Self {
        Self {
            fault,
            message: message.into(),
        }
    }

    /// How the backend failed. Branch on this — never on [`Self::message`].
    pub fn fault(&self) -> PersistenceFault {
        self.fault
    }

    /// What this failure says about the durability of the transaction that
    /// was in flight. Shorthand for `self.fault().durability()`.
    pub fn durability(&self) -> DurabilityOutcome {
        self.fault.durability()
    }

    /// The backend's own message, without the display framing.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The classification travels in the rendered line, not only in the
        // type (#895 §2). In the incident that motivated this, every logged
        // line was redb's `PreviousIo` string — the latch reports — and the
        // one transaction whose durability was actually unknown was
        // indistinguishable from them in the log. An embedder that logs
        // `{err}` now gets `fault=io` (the originating failure, durability
        // unknown) versus `fault=latched` (a report that the handle was
        // already dead and this write was never attempted) for free.
        //
        // `Invariant` renders exactly as it always did: it carries no
        // durability question, and annotating ~200 decode/schema messages
        // with `fault=invariant durability=absent` would bury the two lines
        // that matter under noise.
        if self.fault == PersistenceFault::Invariant {
            return write!(f, "durable-store persistence failure: {}", self.message);
        }
        write!(
            f,
            "durable-store persistence failure [fault={} durability={} reopen={}]: {}",
            self.fault.label(),
            self.fault.durability().label(),
            if self.fault.requires_reopen() {
                "required"
            } else {
                "not-required"
            },
            self.message
        )
    }
}

impl std::error::Error for PersistenceError {}

/// How the durable backend failed, as far as it can honestly be told apart
/// from inside this crate (#895).
///
/// Two questions are answered separately, because they have different
/// answers: [`Self::durability`] (did the in-flight transaction land?) and
/// [`Self::requires_reopen`] (is this handle still usable?).
///
/// `#[non_exhaustive]`: the set is bounded by what the *current* backend
/// can report, and both redb and any future backend can grow states.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PersistenceFault {
    /// The backend refused *before* attempting the operation, because a
    /// previous I/O failure latched the handle (redb `StorageError::
    /// PreviousIo`) or the handle is closed (`DatabaseClosed`). Both come
    /// from `CheckedBackend::check_failure()`, which runs ahead of the
    /// backend op, so this write was never tried: determinate-absent.
    ///
    /// This state is sticky. Every subsequent write on the same handle
    /// reports it until the database is closed and reopened; the incident
    /// behind #895 logged thirty of these in four minutes. Retrying in
    /// place can only produce more of them — see [`Self::requires_reopen`].
    Latched,
    /// The *originating* I/O failure (redb `StorageError::Io`) — the one
    /// that sets the latch. **Not determinate.**
    ///
    /// It covers three underlying states that are indistinguishable from
    /// outside redb, because all three surface as this one variant with the
    /// same errno underneath on a full disk:
    ///
    /// 1. the failure happened before the durability flush, so the
    ///    transaction is absent;
    /// 2. the failure happened *inside* the flush
    ///    (`TransactionalMemory::commit_inner` → `storage.flush()`), which
    ///    issues the real per-page writes in arbitrary iteration order and
    ///    then `sync_data()`s — so whether the header page landed before
    ///    the disk filled is not decidable from here;
    /// 3. the flush returned `Ok` — the transaction *is* durable — and the
    ///    post-durability `resize` that follows it failed, returning `Err`
    ///    to the caller anyway.
    ///
    /// So the honest reading is the conservative union: **may be absent,
    /// may be durable, unknowable from here**. This is reachable on redb
    /// today; it is not a reserved concern for some other backend. Treating
    /// it as "the write failed, retry it" is how an embedder duplicates a
    /// write that is already on disk.
    ///
    /// The resolution is readback, not a decision procedure: durable writes
    /// are keyed (`AcceptWrite`'s intent, `promote_signed`'s `intent_id`),
    /// so after a reopen the caller can read that key back and observe what
    /// actually landed. Correctness in the meantime comes from idempotent
    /// replay, which survives either outcome without deciding which.
    Io,
    /// The backend reports its on-disk structure is corrupt (redb
    /// `StorageError::Corrupted`). Nothing this crate can retry, and the
    /// report says nothing about where the in-flight transaction got to.
    Corrupted,
    /// The value exceeds the backend's hard size limit (redb
    /// `StorageError::ValueTooLarge`). Deterministic and local: the handle
    /// is healthy, the same value will fail identically forever, and no
    /// durable state changed.
    ValueTooLarge,
    /// An internal backend lock was poisoned by a panic in another thread
    /// (redb `StorageError::LockPoisoned`). The handle cannot be trusted,
    /// and the panicking thread may have been mid-commit.
    LockPoisoned,
    /// A backend error variant this version of NMP does not understand.
    ///
    /// Backend error enums can grow without a semver-breaking release. An
    /// unrecognized variant may describe a commit or storage failure, so it
    /// cannot honestly inherit either [`Self::Io`] or [`Self::Invariant`].
    /// Its durability is conservatively unknown and the handle must be
    /// reopened before further use.
    UnknownBackend,
    /// Not a backend failure at all: this crate refusing its own inputs or
    /// its own decoded rows — a decode/encode failure, a schema invariant,
    /// an index disagreement, an exhausted counter. See
    /// [`PersistenceError::invariant`].
    ///
    /// **This, not [`Self::Corrupted`], is where a persisted row that fails
    /// to decode lands (#790.)** The two are not synonyms. `Corrupted` is
    /// redb's own report that the *file structure* is damaged — redb cannot
    /// vouch for what it did or did not write, so it is
    /// [`DurabilityOutcome::Unknown`]. A row that redb returned intact but
    /// whose bytes violate *this crate's* schema is the opposite situation:
    /// the backend is healthy and the failure is raised while decoding.
    /// Every fallible decode and validation precedes the enclosing write
    /// transaction's commit, so the requested mutation provably did not land
    /// and [`DurabilityOutcome::Absent`] is the true claim, not a convenient
    /// one. Reporting such a row as
    /// `Corrupted` would additionally assert [`Self::requires_reopen`],
    /// which is false: reopening the handle cannot change what the row says.
    Invariant,
}

impl PersistenceFault {
    /// What this fault says about the transaction that was in flight.
    ///
    /// [`DurabilityOutcome::Absent`] is a positive claim — "this did not
    /// happen" — so only faults that can prove it get it. A latch is proven
    /// (the backend refused before acting); an oversized value is proven
    /// (redb rejects it on the way in and the transaction is dropped
    /// uncommitted); an invariant is proven because all fallible validation
    /// precedes commit. Corruption, a poisoned lock, and an unrecognized
    /// backend error are not proven: each can describe a thread that was
    /// somewhere inside a commit, and none says where. They join `Io` in the
    /// conservative union rather than claim more than is known.
    ///
    /// `Absent` is only a fact about this store mutation. It is never, by
    /// itself, authority to repeat a signer request, wire send, user action,
    /// allocation, or any other operation. Replay requires a separate
    /// operation-specific recovery proof with retained input, stable
    /// identity, an atomic boundary, and a still-valid typed precondition.
    pub fn durability(self) -> DurabilityOutcome {
        match self {
            Self::Io | Self::Corrupted | Self::LockPoisoned | Self::UnknownBackend => {
                DurabilityOutcome::Unknown
            }
            Self::Latched | Self::ValueTooLarge | Self::Invariant => DurabilityOutcome::Absent,
        }
    }

    /// Whether the store handle must be closed and reopened before any
    /// further write can succeed.
    ///
    /// This is never a licence to retry the failed commit against the
    /// *same* handle. redb sets `needs_recovery` on the transactional
    /// memory for any commit error and `commit_inner` opens by asserting
    /// that flag is clear, so an in-place commit retry panics rather than
    /// erroring. Reopen is the only safe recovery, and `nmp-store` exposes
    /// no retry door of any kind.
    pub fn requires_reopen(self) -> bool {
        match self {
            Self::Latched
            | Self::Io
            | Self::Corrupted
            | Self::LockPoisoned
            | Self::UnknownBackend => true,
            Self::ValueTooLarge | Self::Invariant => false,
        }
    }

    /// Stable lowercase token for logs. Not a display string for humans —
    /// [`PersistenceError`]'s `Display` embeds this.
    pub fn label(self) -> &'static str {
        match self {
            Self::Latched => "latched",
            Self::Io => "io",
            Self::Corrupted => "corrupted",
            Self::ValueTooLarge => "value-too-large",
            Self::LockPoisoned => "lock-poisoned",
            Self::UnknownBackend => "unknown-backend",
            Self::Invariant => "invariant",
        }
    }
}

/// What a [`PersistenceError`] says about whether the in-flight transaction
/// reached durable storage (#895).
///
/// `#[non_exhaustive]`: a third state — determinate-durable-but-errored — is
/// real on redb today (a post-flush `resize` failure returns `Err` on an
/// already-durable transaction) but is not distinguishable from the other
/// `Io` cases from outside redb, so it is folded into [`Self::Unknown`]
/// rather than claimed. If it ever becomes observable it arrives as a new
/// variant, not as a silent change of meaning for these two.
///
/// This axis is reachable on redb today. It is not a reserved concern for a
/// future backend, and reading it that way is how an embedder concludes an
/// `Io` is determinate and builds the wrong retry.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DurabilityOutcome {
    /// Nothing was written. The backend either refused before attempting
    /// the operation or failed before the durability flush.
    Absent,
    /// Not determinate. The transaction may be absent, or it may already be
    /// durable. Do not retry it as though it were absent — replay it
    /// idempotently, or reopen and read the key back.
    Unknown,
}

impl DurabilityOutcome {
    /// Stable lowercase token for logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Unknown => "unknown",
        }
    }
}

/// A stored event plus its provenance. What `query` returns — every caller
/// gets provenance for free, never a bare `Event` (ledger #5's falsifier:
/// no `query` path returns an event without its provenance populated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    pub event: Event,
    pub provenance: Provenance,
}

/// The closed canonical continuation key for newest-first event selection.
///
/// Store pages are ordered by `created_at` descending, then event id
/// ascending. A cursor is exclusive: the next page may contain exactly rows
/// whose timestamp is lower, or whose timestamp is equal and id is greater.
/// Keeping both protocol facts in one typed key prevents callers from
/// approximating a continuation by decrementing Nostr's one-second timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventCursor {
    pub created_at: Timestamp,
    pub event_id: EventId,
}

impl EventCursor {
    pub const fn new(created_at: Timestamp, event_id: EventId) -> Self {
        Self {
            created_at,
            event_id,
        }
    }

    pub fn from_event(event: &Event) -> Self {
        Self::new(event.created_at, event.id)
    }

    fn admits(&self, event: &Event) -> bool {
        event.created_at < self.created_at
            || (event.created_at == self.created_at && event.id > self.event_id)
    }
}

/// Which relay delivered an event, and the engine's wall-clock time at
/// receipt — the `insert` door's second argument (M3 §3.1's `from:
/// RelayObserved`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayObserved {
    pub relay: RelayUrl,
    pub at: Timestamp,
}

impl RelayObserved {
    pub fn new(relay: RelayUrl, at: Timestamp) -> Self {
        Self { relay, at }
    }
}

/// The result of an [`EventStore::insert`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    /// Brand-new event id, not part of any replaceable/addressable
    /// competition (or the first event at that address).
    Inserted,
    /// This exact event id is already present. `provenance_grew` is `true`
    /// iff the merge actually changed the provenance map (M1's no-op stub
    /// becomes a real merge in M3 — ledger #5).
    Duplicate {
        provenance_grew: bool,
        /// Locally-accepted intent owners that this verified relay copy
        /// atomically advanced from Pending to Signed. The engine must route
        /// each matching obligation exactly once; an empty set is the common
        /// ordinary-dedup case.
        satisfied_intents: Vec<IntentId>,
        /// Whether the row this delivery merged into is one this node
        /// accepted itself (`Provenance.local.is_some()`). A relay echo never
        /// changes it, and it is not `!satisfied_intents.is_empty()`: an
        /// already-signed row, and a row whose owners were all compensated
        /// away, satisfy no intent and are still ours. Pinned projections
        /// need it to evaluate [`Provenance::visible_under_pin`] over the
        /// committed delta without re-reading the row.
        locally_accepted: bool,
    },
    /// A replaceable/addressable winner changed. `replaced` is the evicted
    /// row itself, handed back whole: the store is holding it at the exact
    /// moment of eviction, and this is the only moment it can be returned
    /// (retraction-and-negative-deltas.md §1.1) — the resolver's dirty-seed
    /// and the optimistic-write rollback path both need to `match_event`
    /// and re-insert this row after the store has already dropped it.
    Superseded {
        /// The full row that was superseded (dropped from the store).
        /// Boxed so the common `Inserted`/`Duplicate`/`Stale` variants stay
        /// small — `Superseded` is the rare, eviction-only case.
        replaced: Box<StoredEvent>,
    },
    /// This event is older than the current winner for its
    /// replaceable/addressable address (or ties on `created_at` but does not
    /// win the lexicographic id tie-break). Rejected: dropped, never stored.
    Stale,
    /// Refused at the door: never stored, nothing to retract
    /// (retraction-and-negative-deltas.md §1.1/§2/§3).
    Refused(RefuseReason),
    /// A kind:5 (NIP-09) deletion event, stored normally like any other
    /// regular event — kind:5 is outside M1's replaceable/addressable set,
    /// so its own storage is always plain `Inserted` by construction, and
    /// this variant is returned in place of `Inserted` only for that one
    /// case. `deleted` holds every currently-held target this deletion
    /// actually removed (author-verified against this event's own pubkey),
    /// handed back whole — the only moment the door can return them,
    /// mirroring `Superseded { replaced }` (retraction-and-negative-
    /// deltas.md §2).
    Kind5Processed { deleted: Vec<StoredEvent> },
}

/// Why an [`EventStore::insert`] refused an event outright, before it ever
/// touched an index.
///
/// Serialized because a locally-authored write refused at acceptance is
/// still taken into custody: it becomes a one-row, permanently-failed
/// publish-queue entry carrying this reason ([`EventStore::accept_refused`]),
/// so the reason has to survive a restart exactly as it was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefuseReason {
    /// The event's NIP-40 `expiration` tag is already in the past at the
    /// moment of insert (checked against the `RelayObserved` clock the
    /// caller passed in). Wired in this unit.
    AlreadyExpired,
    /// The event's id (or, for an addressable/replaceable target, its
    /// address) was tombstoned by an earlier verified kind:5 deletion from
    /// the same author (retraction-and-negative-deltas.md §2, §7:
    /// tombstone retention is PERMANENT — never GC-claimed).
    Tombstoned,
    /// A whole-value replacement was composed from `expected`, but the
    /// canonical winner at that exact replaceable/addressable coordinate
    /// was `actual` when the store's atomic acceptance transaction ran.
    /// Nothing was stored or journaled and no ids were allocated.
    ReplaceableBaseChanged {
        expected: Option<EventId>,
        actual: Option<EventId>,
    },
    /// A caller attached a replaceable-base precondition to an event kind
    /// that has no replaceable/addressable coordinate. Fail closed instead
    /// of silently accepting an unchecked write.
    ReplaceableBaseOnRegularEvent,
}

/// Why an [`EventStore::remove`] call is removing a row. Exists so
/// diagnostics can count retractions per cause, and so `remove` reads as
/// self-documentingly *not* a general delete API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetractReason {
    /// An optimistic local write was rejected (or its whole intent failed)
    /// before ever being accepted.
    Rejected,
    /// Removed by a verified kind:5 deletion from the event's own author.
    Deleted,
    /// Removed because its NIP-40 `expiration` deadline passed.
    Expired,
}

/// Journal-level signature state of an `PUBLISH_QUEUE_INTENTS` row (Fable
/// checkpoint R1) — a FINER granularity than the row-level [`SigState`]
/// the app sees: `AwaitingSigner` and `Pending` both project as
/// `SigState::Pending` to the app (both are "not yet signed"), but the
/// engine needs the extra distinction on restart to know whether a signer
/// attach should re-trigger `RequestSign` (`AwaitingSigner`) or whether a
/// sign request was already in flight and its response is simply lost
/// (`Pending` — safe to re-request; double-signing after a crash is
/// harmless, same id either valid signature promotes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntentSigState {
    /// No signer for `expected_pubkey` was attached at acceptance.
    AwaitingSigner,
    /// A signer is (or was) in flight; the row's `sig` is still
    /// [`sentinel_signature`].
    Pending,
    /// [`EventStore::promote_signed`] has run; the row carries a real
    /// signature.
    Signed,
}

/// The full journal payload for one locally-accepted write intent (Fable
/// checkpoint R7): everything #3's "one crash-atomic commit" enumerates,
/// gathered into one struct so `accept_write` can commit it and the pending
/// row in a single `redb::WriteTransaction` — atomicity is structural, not
/// a calling convention.
///
/// NOTE: neither an `IntentId` nor a receipt id is a field here — the store
/// allocates BOTH, from durable high-water marks bumped inside this same
/// transaction, and hands both back on every journaled [`AcceptOutcome`]
/// variant. See [`IntentId`]'s doc for why a caller-supplied id of either
/// kind is unsound: issue #3's "receipt ids remain stable and unique
/// across restart" carries the IDENTICAL reuse hazard the moment receipts
/// are durably retained across restart (architecture review correction) —
/// an engine-side counter that resets on restart could hand out a receipt
/// id colliding with a retained `PUBLISH_QUEUE_RECEIPTS` row, making
/// `reattach_receipt` ambiguous.
pub struct AcceptWrite {
    /// The frozen, unsigned NIP-01 body: pubkey/created_at/kind/tags/
    /// content are final and `event.id` is already `EventId::new(..)` over
    /// exactly those fields (the signature is not an id input — Q1).
    /// `event.sig` must be [`sentinel_signature`] until
    /// [`EventStore::promote_signed`] swaps in the real one.
    pub frozen: Event,
    /// Optional compare-and-swap guard for a whole-value replacement. The
    /// store derives the coordinate from `frozen` and compares its current
    /// canonical winner inside the same transaction that would accept the
    /// new row. `Some(None)` means the caller observed no local base;
    /// `None` means this is an ordinary, unconditional write.
    pub replaceable_base: Option<Option<EventId>>,
    /// The app stated no `created_at`, so this write's timestamp is NMP's
    /// to decide and `frozen.created_at` currently holds nothing but the
    /// caller's clock. Inside the same transaction that compares
    /// `replaceable_base` — and against the very row it compares — the
    /// store moves the stamp forward to `winner.created_at + 1` whenever
    /// the clock is not already ahead (the `max(clock, winner + 1)` rule)
    /// and re-derives `frozen.id` over the stamped body.
    ///
    /// This is why the rule lives here rather than in the engine: a
    /// timestamp computed outside the transaction is computed against a row
    /// that may already have moved, which is exactly the seam the
    /// precondition exists to close. `false` when the app stated its own
    /// `created_at` — present-then-changed is the one thing a stated field
    /// may never be, even when the value it stated loses the race.
    pub monotonic_stamp: bool,
    /// The pinned signing identity (#43 "pins the chosen identity at
    /// acceptance"). Ordinarily equal to `frozen.pubkey`; kept as an
    /// explicit field because it is a distinct journal fact (#2's "expected
    /// pubkey"), not merely derivable convenience.
    pub expected_pubkey: PublicKey,
    /// Opaque placeholder the store persists and returns verbatim — #47
    /// gives it real meaning; this frame only pins the persistence hook
    /// (Fable checkpoint Q5).
    pub signing_identity_ref: String,
    /// Opaque, engine-owned routing snapshot at acceptance — persisted and
    /// returned verbatim by `recover_publish_queue`. The store never interprets
    /// routing semantics; §5's append-only-revision ownership stays in
    /// `nmp-engine`.
    pub routing: String,
    /// The intent's sig state AT ACCEPTANCE — always `AwaitingSigner` or
    /// `Pending`, never `Signed` (a row only reaches `Signed` through
    /// `promote_signed`).
    pub sig_state: IntentSigState,
    pub accepted_at: Timestamp,
    /// #591 crash-safe correlation token. When `Some`, checked (and, on a
    /// first sighting, journaled) inside this SAME acceptance transaction
    /// -- see [`EventStore::accept_write`]'s doc for the exact protocol.
    pub correlation: Option<nmp_grammar::CorrelationToken>,
}

/// The result of an [`EventStore::accept_write`] call — mirrors
/// [`InsertOutcome`]'s shape (Fable checkpoint: "reuses the widened
/// `Superseded` shape so the resolver sorts it exactly like a relay
/// insert"), including `Kind5Processed`: a locally-composed kind:5 draft
/// immediately, in the SAME transaction, stages a REVERSIBLE suppression
/// claim over every target it names — hiding whatever row currently lives
/// there from `query` WITHOUT moving or removing it (architecture review
/// correction — issue #2's "no app optimistic mirror" promise extends to
/// local deletions too). This replaced an earlier, withdrawn design that
/// physically moved a target row into a per-intent stash: codex-nova found
/// that made the target's OWN `promote_signed`/`compensate_write` blind to
/// it (a stashed row is invisible to anyone searching `EVENTS`/
/// `PUBLISH_QUEUE_DISPLACED`), and made an exact-`Duplicate` kind:5 intent's
/// promotion unsound (promoting it committed a real, permanent deletion
/// with no stash of its own to drop). The suppression-claim model fixes
/// both: rows never move, so every other door keeps working on exactly
/// the row it always did — a claim is pure, reversible metadata.
/// `compensate_write` drops a still-pending intent's claims outright (the
/// target reappears immediately — nothing to re-insert, it never left);
/// `promote_signed` drops them AND commits the deletion for real (the same
/// author-verified tombstone-write processing `insert` runs for a
/// relay-observed kind:5) — permanent from that point on
/// (retraction-and-negative-deltas.md §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptOutcome {
    /// Brand-new pending row, no address competition. `intent_id`/
    /// `receipt_id` are the store-allocated ids (see [`IntentId`]'s doc) —
    /// the ONLY place a caller learns either.
    Inserted {
        intent_id: IntentId,
        receipt_id: u64,
        row: StoredEvent,
    },
    /// This exact event id was already held (see `Provenance::local_origin`'s
    /// doc — an edge case, not the relay-echo hand-off, which goes through
    /// ordinary `insert`/dedup instead). Still allocates and journals a
    /// fresh `intent_id`/`receipt_id` — this call is still a distinct
    /// accepted intent, joining the existing row's owner set (issue #2's
    /// ownership-set model — see `LocalOrigin`'s doc) rather than being
    /// silently discarded. If the existing row (locally owned OR purely
    /// relay-observed — either way its `event.sig` is already real, not a
    /// sentinel) is ALREADY signed, this intent's OWN journal/receipt are
    /// journaled `Signed` from the start rather than `Pending` (codex-nova
    /// ruling): an offline co-owner signer must never strand a receipt
    /// behind an event that's already validly signed, and there is
    /// nothing left for this intent to sign.
    Duplicate {
        intent_id: IntentId,
        receipt_id: u64,
        row: StoredEvent,
    },
    /// The pending row won a replaceable/addressable address, evicting
    /// `replaced` — durably stashed by the caller into `PUBLISH_QUEUE_DISPLACED`
    /// in the SAME transaction, so pre-signature compensation
    /// (`compensate_write`) can restore it (retraction doc §4.2).
    Superseded {
        intent_id: IntentId,
        receipt_id: u64,
        row: StoredEvent,
        replaced: Box<StoredEvent>,
        /// Older open delivery obligations at this exact address that had
        /// not started a wire attempt and were retired atomically with this
        /// acceptance. Their retained receipts remain reattachable.
        retired: Vec<RetiredIntent>,
    },
    /// This intent lost its address race to an existing, newer winner.
    /// The intent is still journaled (still gets signed and delivered —
    /// only `Refused` below skips the journal) but produces no pending row.
    Stale {
        intent_id: IntentId,
        receipt_id: u64,
    },
    /// A locally-composed kind:5 (NIP-09) deletion, stored like any other
    /// pending row through this door AND, in the SAME transaction, staging
    /// a provisional suppression claim over every target it names — the
    /// targets disappear from `query` immediately, before any relay
    /// round-trip (architecture review correction: issue #2's "no app
    /// optimistic mirror" promise extends to locally-composed deletions
    /// too), without being moved or removed. `hidden` holds every
    /// currently-visible row this claim just hid — both e-tag id targets
    /// and, unlike the deferred-to-promotion treatment an earlier
    /// revision gave them, a-tag address targets' current winners too
    /// (suppression is cheap and reversible either way, so there is no
    /// reason left to defer). Returned in place of `Inserted` only for
    /// this one case — kind:5 has no replaceable/addressable address, so
    /// it can never reach `Superseded`/`Stale` by construction.
    Kind5Processed {
        intent_id: IntentId,
        receipt_id: u64,
        row: StoredEvent,
        hidden: Vec<StoredEvent>,
    },
    /// Refused at the door — the same tombstone/expiry refusal `insert`
    /// runs. Terminal typed failure to the caller (R3): NOTHING is
    /// journaled — no intent row, no pending row, no receipt residue, and
    /// (correspondingly) no `IntentId`/receipt id is ever allocated for a
    /// refused call, so refusal can never "burn" either.
    Refused(RefuseReason),
}

/// One older replaceable/addressable obligation atomically retired when a
/// newer winner was accepted. This is acceptance evidence for the engine,
/// not another app-facing workload noun.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredIntent {
    pub intent_id: IntentId,
    pub receipt_id: u64,
}

impl AcceptOutcome {
    /// The `IntentId` this call journaled, if any — `None` only for
    /// `Refused` (R3: nothing was ever journaled, and no id was ever
    /// allocated for a refused call).
    pub fn journaled_intent_id(&self) -> Option<IntentId> {
        match self {
            AcceptOutcome::Inserted { intent_id, .. }
            | AcceptOutcome::Duplicate { intent_id, .. }
            | AcceptOutcome::Superseded { intent_id, .. }
            | AcceptOutcome::Stale { intent_id, .. }
            | AcceptOutcome::Kind5Processed { intent_id, .. } => Some(*intent_id),
            AcceptOutcome::Refused(_) => None,
        }
    }

    /// The store-allocated receipt id this call journaled, if any — `None`
    /// only for `Refused` (architecture review correction: receipt ids are
    /// store-allocated the same way `IntentId` is, and a refusal burns
    /// neither).
    pub fn journaled_receipt_id(&self) -> Option<u64> {
        match self {
            AcceptOutcome::Inserted { receipt_id, .. }
            | AcceptOutcome::Duplicate { receipt_id, .. }
            | AcceptOutcome::Superseded { receipt_id, .. }
            | AcceptOutcome::Stale { receipt_id, .. }
            | AcceptOutcome::Kind5Processed { receipt_id, .. } => Some(*receipt_id),
            AcceptOutcome::Refused(_) => None,
        }
    }

    /// The canonical row this acceptance is about, when it produced one.
    ///
    /// Its `event` is the body the store actually froze — which is not
    /// always the body the caller handed in: an [`AcceptWrite`] with
    /// `monotonic_stamp` set may have had its `created_at` moved forward
    /// inside the transaction, re-deriving the id. A caller that needs the
    /// frozen body (to hand it to a signer, to name it on a receipt) must
    /// read it from here rather than from what it sent.
    ///
    /// `None` for `Stale` — which lost its address race and owns no row —
    /// and for `Refused`, which journaled nothing. Neither is reachable for
    /// a `monotonic_stamp` write whose precondition passed: the stamp is
    /// strictly greater than the winner it was compared against, so the
    /// candidate cannot then lose to that same winner.
    pub fn accepted_row(&self) -> Option<&StoredEvent> {
        match self {
            AcceptOutcome::Inserted { row, .. }
            | AcceptOutcome::Duplicate { row, .. }
            | AcceptOutcome::Superseded { row, .. }
            | AcceptOutcome::Kind5Processed { row, .. } => Some(row),
            AcceptOutcome::Stale { .. } | AcceptOutcome::Refused(_) => None,
        }
    }
}

/// The result of an [`EventStore::promote_signed`] call — keyed by
/// `IntentId`, not the frozen event's id (architecture review correction: a
/// `Duplicate`/`Stale` intent with no shared row never won a live row at
/// its own id at all, and a once-live row can since have been superseded,
/// kind:5-deleted, or expired). Three cases, all reachable: `intent_id` is
/// a MEMBER of a live row's owner set (issue #2, team-lead decision —
/// ownership is a SET, so an exact `Duplicate` sharing an already-locally-
/// owned row is a CO-OWNER of it, not a row of its own) — sentinel swapped
/// for `sig` in place, same id, same EVENTS/ADDR_INDEX/BY_AUTHOR/BY_KIND/BY_TAG
/// entries, zero churn; `intent_id` is a member of some OTHER intent's
/// `PUBLISH_QUEUE_DISPLACED` stash entry's owner set (chained local supersession
/// before this intent could sign — the real signature is synced into that
/// stash entry too, so a future restore of it never resurrects a stale
/// sentinel copy of an intent that actually signed); or neither (the row
/// is gone for some unrelated reason — relay supersession, kind:5
/// deletion, NIP-40 expiry — and the signed bytes are synthesized from the
/// journal's own copy so the engine can still publish them even though
/// this intent wins no local address).
///
/// codex-nova ruling (issue #2's ownership-set model, tightened after
/// review): the FIRST owner to sign atomically transitions EVERY other
/// co-owner's own `PUBLISH_QUEUE_INTENTS`/`PUBLISH_QUEUE_RECEIPTS` row to `Signed`
/// against the SAME canonical bytes, in this SAME call — never lazily,
/// deferred until (or unless) each co-owner separately calls
/// `promote_signed` itself. An offline co-owner signer that never calls
/// back must not strand its receipt behind an event that is already
/// validly signed. `co_signed` names every OTHER intent this call just
/// advanced this way, so the caller can advance each of THEIR routing
/// obligations too, not only `intent_id`'s own. A co-owner's OWN later
/// call (e.g. its signer's delayed callback) now correctly answers
/// `NotFound` — its journal is already `Signed` by the time it calls, so
/// the existing per-intent guard catches it (see `NotFound`'s doc).
///
/// Either way, `SigState`/`IntentSigState` flip to `Signed`, the durable
/// `PUBLISH_QUEUE_DISPLACED` stash for `intent_id` AND every co-owner named in
/// `co_signed` is deleted in the same transaction (R6), and — if this was
/// a pending kind:5 draft — every owner's suppression claims become
/// authoritative permanent tombstones together. Boxed for the same reason
/// `InsertOutcome::Superseded` is: keeps the common `NotFound` variant
/// small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromoteOutcome {
    Promoted {
        row: Box<StoredEvent>,
        /// Every OTHER co-owner `IntentId` this call ALSO atomically
        /// transitioned to `Signed` against the SAME canonical bytes (see
        /// this enum's own doc for why) — empty when `intent_id` is the
        /// row's only owner, which is the common case.
        co_signed: Vec<IntentId>,
    },
    /// This `IntentId` names no still-open intent, OR its OWN journal is
    /// ALREADY `Signed` — either because it promoted before (codex-nova's
    /// original repeat-promotion finding), or because some OTHER co-owner
    /// promoted first and this call's `co_signed` already advanced it
    /// (this intent's own delayed signer callback arriving after the
    /// fact). Also covers already compensated, or never accepted through
    /// `accept_write`.
    NotFound,
}

/// The result of an [`EventStore::compensate_write`] call — keyed by
/// `IntentId`, same three-case dispatch [`PromoteOutcome`] documents (live
/// row / displaced-in-another-intent's-stash / neither), same ownership-SET
/// model (issue #2, team-lead decision). If live, `intent_id` is removed
/// from the row's owner set; the row is only actually `remove(id,
/// Rejected)`-ed (no tombstone — the row was never validly signed) once
/// the set is EMPTY, `SigState` is still `Pending`, AND no relay has
/// independently confirmed it — an exact `Duplicate`'s still-open
/// obligation, an already-`Signed` state some OTHER co-owner committed, or
/// independent relay provenance, all survive THIS one intent's
/// cancellation (see `LocalOrigin`'s doc). If sitting in another intent's
/// stash, the SAME conditional removal applies to that stash entry's
/// owner set instead of dropping it outright. Either way, THIS intent's
/// own displaced predecessor (if any) is restored through the same one
/// door and returned here (`None` if it displaced nothing, or the
/// re-offered predecessor came back `Stale` — retraction doc §3.4).
/// If this was a pending kind:5 draft, this intent's OWN suppression
/// claims are dropped outright — every target it named reappears in
/// `query` immediately, with `revealed` listing the ones that ACTUALLY
/// became newly visible: a true visibility DELTA (architecture review
/// correction), computed from before/after suppression state and deduped
/// by event id, so a target still hidden by some OTHER intent's
/// overlapping claim, one already permanently removed by an intent that
/// promoted its own deletion of the same target, or one this claim's own
/// author/ceiling component never actually covered in the first place, is
/// correctly excluded. Nothing is ever re-inserted for `revealed`: a
/// suppressed row never left `EVENTS` in the first place — cancelling a
/// delete brings the content back, not merely closes the journal. The
/// intent's `PUBLISH_QUEUE_INTENTS`/`PUBLISH_QUEUE_DISPLACED`/suppression-claim rows
/// were all deleted in the same transaction. Boxed for the same reason
/// `InsertOutcome::Superseded` is: keeps the common `NotFound` variant
/// small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompensateOutcome {
    Compensated {
        restored: Option<Box<StoredEvent>>,
        revealed: Vec<StoredEvent>,
    },
    /// The intent crossed signature promotion; the destructive pre-signature
    /// door refuses without changing its row, receipt, or lanes.
    AlreadySigned,
    /// This `IntentId` names no still-open intent: already compensated or
    /// never accepted through `accept_write`.
    NotFound,
}

/// Typed result from the receipt-keyed queue-entry removal door
/// ([`EventStore::remove_publish_queue_entry`], #1039).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveQueueEntryOutcome {
    Removed,
    NotFound,
    /// The receipt still owns an open `PUBLISH_QUEUE_INTENTS` row. Removal is
    /// for entries nothing is going to move; an intent with live work is
    /// cancelled, not removed.
    StillOpen,
}

/// One still-open intent replayed by [`EventStore::recover_publish_queue`] on
/// boot. The pending row itself is NOT re-inserted — it is already live in
/// the store (committed atomically at `accept_write` time) and query-visible
/// from the first post-boot subscription; this is only the journal metadata
/// `nmp-engine` needs to rebuild its in-memory `PendingWrite`/
/// `event_to_receipt` bookkeeping (plan §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishQueueIntent {
    pub intent_id: IntentId,
    pub receipt_id: u64,
    pub frozen: Event,
    pub expected_pubkey: PublicKey,
    pub signing_identity_ref: String,
    pub routing: String,
    pub sig_state: IntentSigState,
    /// The predecessor this intent displaced, if any — still durable
    /// (`PUBLISH_QUEUE_DISPLACED` is deleted only by `promote_signed` or
    /// `compensate_write`, never by `recover_publish_queue`), so a post-restart
    /// cancellation can still restore it.
    pub displaced: Option<StoredEvent>,
    pub accepted_at: Timestamp,
}

/// A durably-retained receipt's coarse status — the STORE-OBSERVABLE
/// subset of the full receipt stream (`nmp-engine`'s `WriteFact` owns
/// the complete enum, including per-relay `Routed`/`Sent`/`Acked`/
/// `Rejected`/`GaveUp`/`Failed`; this crate only knows what its OWN four
/// doors did to a receipt). Retained under `PUBLISH_QUEUE_RECEIPTS` — separately
/// from `PUBLISH_QUEUE_INTENTS`'s open-work row — precisely so a receipt stays
/// reattachable via [`EventStore::reattach_receipt`] after the open-work
/// row is gone (architecture review correction: R8-style terminal cleanup
/// of `PUBLISH_QUEUE_INTENTS` must never also delete receipt identity/state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptState {
    /// `accept_write` ran; nothing else has happened to this receipt yet.
    Accepted,
    /// `promote_signed` ran; the row carries a real signature. (Per-relay
    /// delivery evidence beyond this point is a later unit's job — the
    /// durable attempt table this frame only creates the schema for.)
    Signed,
    /// `compensate_write` ran; the pending row was retracted pre-signature
    /// (retraction doc §4.2). Terminal — a compensated intent never
    /// promotes.
    Compensated,
    /// The app explicitly cancelled this still-unsigned obligation. The
    /// compensation transaction committed, so this is a durable terminal
    /// fact rather than a generic failure string.
    Cancelled,
    /// A newer accepted event won the same NIP-01 replaceable/addressable
    /// coordinate before this obligation started any wire attempt. Terminal:
    /// the receipt is retained, but the old intent is absent from recovery.
    Superseded,
    /// Routing finished — knowledge exhausted — and named zero relays, so
    /// there was nowhere to publish ([`EventStore::close_unroutable_intent`]).
    /// Terminal, and distinct from [`Self::Refused`]: the instruction was
    /// fine and the store took it; the WORLD had no destination for it.
    /// Retained so a reattaching app is told that, rather than told nothing.
    NoDestination,
    /// The acceptance instruction was answered with a semantic no
    /// ([`EventStore::accept_refused`]): the store was working and said no.
    /// Terminal at birth — there was never an intent, a journal row, a
    /// signer request or a relay write, only this one retained receipt.
    ///
    /// The write is still in CUSTODY: the app reads the reason back through
    /// reattachment or enumeration, and a
    /// [`RefuseReason::ReplaceableBaseChanged`] carries both event ids so
    /// the app can fetch what is actually there, reapply the user's change
    /// and resubmit without ever troubling them.
    Refused(RefuseReason),
}

/// Backend-extension vocabulary for why the one atomic compensation
/// transaction is running. This is not a third app-facing workload noun:
/// [`EventStore`] remains implementable outside this crate, so its shared
/// implementation door must be nameable by adapter stores. The exhaustive
/// enum admits only the two legal terminal outcomes instead of exposing a
/// `ReceiptState` parameter that could persist an impossible transition.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompensationReason {
    Failure,
    ExplicitCancellation,
}

/// A durably-retained receipt record, independent of whether the intent's
/// open-work row (`PUBLISH_QUEUE_INTENTS`/[`PublishQueueIntent`]) still exists —
/// see [`ReceiptState`]'s doc for why this separation exists. This unit
/// builds no pruning policy for these rows (mirrors how the retry-owner
/// follow-up, not this frame, owns `PUBLISH_QUEUE_ATTEMPTS` retention policy);
/// they simply accumulate until a later unit defines a retention/GC rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishQueueReceipt {
    pub receipt_id: u64,
    /// `Some` for a receipt backed by a real (open or since-closed)
    /// `accept_write` intent. `None` for a receipt-ONLY record
    /// ([`EventStore::accept_refused`]): a write refused at the acceptance
    /// door still enters custody as one retained, reattachable receipt,
    /// without ever gaining a journal row, a pending event row, a signer
    /// request or a relay write.
    pub intent_id: Option<IntentId>,
    pub frozen_id: EventId,
    pub expected_pubkey: PublicKey,
    pub state: ReceiptState,
}

/// Versioned, durable evidence for one publication attempt. The key is the
/// full `(intent, relay, ordinal)` tuple: a restart can never confuse a new
/// send with an older ambiguous send, and the exact signed bytes are retained
/// rather than reconstructed from mutable routing state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishQueueAttempt {
    pub version: u8,
    pub intent_id: IntentId,
    pub relay: RelayUrl,
    pub ordinal: u64,
    pub event: Event,
    pub outcome: PublishQueueAttemptOutcome,
}

/// Stable identity of one durable publication lane.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PublishQueueLaneKey {
    pub intent_id: IntentId,
    pub relay: RelayUrl,
}

/// The current, versioned cursor for one `(intent, relay)` obligation.
/// History remains in the route/attempt/detail tables; this is the bounded
/// authoritative row recovery and scheduling read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishQueueLane {
    pub version: u8,
    pub key: PublishQueueLaneKey,
    pub revision: u64,
    pub last_ordinal: u64,
    pub state: PublishQueueLaneState,
}

/// The typed source of a terminal authentication refusal.
///
/// This vocabulary is deliberately source-neutral: a local policy or signer
/// refusal is not a relay rejection merely because it prevents a relay write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthDenialSource {
    Policy,
    Signer,
    Relay,
}

/// Durable authentication-refusal evidence owned by one exact write lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthDenial {
    pub source: AuthDenialSource,
    pub reason: String,
}

/// Terminal lane vocabulary.
///
/// Unlike an attempt terminal, a true AUTH denial can finish a lane before
/// the first EVENT attempt exists (ordinal zero). Keeping this separate from
/// [`PublishQueueAttemptOutcome`] makes `Started` structurally impossible in a terminal
/// lane and avoids inventing an attempt merely to retain a denial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishQueueTerminalOutcome {
    Acked,
    Rejected(String),
    GaveUp,
    AuthDenied(AuthDenial),
}

impl PublishQueueTerminalOutcome {
    fn from_attempt(outcome: PublishQueueAttemptOutcome) -> Result<Self, PersistenceError> {
        match outcome {
            PublishQueueAttemptOutcome::Started => Err(PersistenceError::invariant(
                "Started is not a terminal lane outcome",
            )),
            PublishQueueAttemptOutcome::Acked => Ok(Self::Acked),
            PublishQueueAttemptOutcome::Rejected(reason) => Ok(Self::Rejected(reason)),
            PublishQueueAttemptOutcome::GaveUp => Ok(Self::GaveUp),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishQueueLaneState {
    WaitingConnection,
    WaitingAuth,
    Eligible {
        since: Timestamp,
    },
    InFlight {
        ordinal: u64,
        phase: PublishQueueInFlightPhase,
    },
    Transient {
        ordinal: u64,
        eligible_at: Timestamp,
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
    },
    Terminal {
        ordinal: u64,
        outcome: PublishQueueTerminalOutcome,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishQueueInFlightPhase {
    AwaitingHandoff,
    AwaitingAck { deadline: Timestamp },
}

/// Ordered deadline-index discriminator. Retry eligibility and ACK timeout
/// share one index but remain impossible to conflate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishQueueDeadlineKind {
    RetryEligible,
    AckTimeout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishQueueDeadline {
    pub at: Timestamp,
    pub key: PublishQueueLaneKey,
    pub lane_revision: u64,
    pub kind: PublishQueueDeadlineKind,
}

/// Transport handoff evidence, deliberately independent of nmp-transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandoffEvidence {
    NotHandedOff,
    Written,
    Ambiguous,
}

/// Closed persistence vocabulary selected by the engine. The store never
/// maps transport outcomes into one of these causes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishQueueTransientCause {
    Interrupted,
    AckTimeout,
    ConnectionLost,
    RelayRateLimited,
    RelayError,
    AuthRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishQueueAttemptHandoff {
    pub at: Timestamp,
    pub result: HandoffEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishQueueAttemptTransient {
    pub eligible_at: Timestamp,
    pub cause: PublishQueueTransientCause,
    pub raw_reason: Option<String>,
}

/// The current evidence row beside an immutable `Started` attempt row. Every
/// attempt in the current schema has exactly one of these; there is no
/// pre-detail attempt shape to adopt or synthesize a shell for (#867).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishQueueAttemptDetails {
    pub version: u8,
    pub intent_id: IntentId,
    pub relay: RelayUrl,
    pub ordinal: u64,
    pub started_at: Option<Timestamp>,
    pub handoff: Option<PublishQueueAttemptHandoff>,
    #[serde(default)]
    pub transient: Option<PublishQueueAttemptTransient>,
    pub finished_at: Option<Timestamp>,
    pub terminal: Option<PublishQueueAttemptOutcome>,
}

pub(crate) fn attempt_is_live(
    attempt: &PublishQueueAttempt,
    details: Option<&PublishQueueAttemptDetails>,
) -> bool {
    if attempt.outcome != PublishQueueAttemptOutcome::Started {
        return false;
    }
    match details {
        Some(details) if details.terminal.is_some() || details.transient.is_some() => false,
        Some(details)
            if matches!(
                details.handoff,
                Some(PublishQueueAttemptHandoff {
                    result: HandoffEvidence::NotHandedOff,
                    ..
                })
            ) =>
        {
            false
        }
        _ => true,
    }
}

/// Caller-selected post-handoff persistence state. This is a fact-writing
/// vocabulary, not a classification policy: the engine chooses the variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishQueuePostHandoffState {
    WaitingConnection,
    WaitingAuth,
    Eligible {
        since: Timestamp,
    },
    AwaitingAck {
        deadline: Timestamp,
    },
    Transient {
        eligible_at: Timestamp,
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
    },
    Terminal {
        outcome: PublishQueueAttemptOutcome,
        finished_at: Timestamp,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseIntentOutcome {
    Closed,
    AlreadyClosed,
}

/// One append-only snapshot of the exact relay set resolved for an intent.
/// It is committed before any corresponding attempt may start, so a failed
/// attempt-start cannot erase the lane across restart when dynamic directory
/// state is empty or has changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishQueueRouteRevision {
    pub version: u8,
    pub intent_id: IntentId,
    pub ordinal: u64,
    pub relays: BTreeSet<RelayUrl>,
}

/// Effective attempt state. Base rows record `Started` before the engine emits
/// `PublishEvent` and are never rewritten; terminal variants are overlaid from
/// the required detail row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishQueueAttemptOutcome {
    Started,
    Acked,
    Rejected(String),
    GaveUp,
}

/// The single mutating door onto the event store.
pub trait EventStore {
    /// Insert an event observed via `from`. An already-expired event (NIP-40,
    /// judged against `from.at`) is `Refused` before anything else runs —
    /// never stored, nothing to retract. Otherwise dedup-by-id FIRST — on a
    /// hit, merge `from` into the existing row's provenance and return
    /// `Duplicate{provenance_grew}` with NO index churn. Next, a tombstone
    /// check (retraction-and-negative-deltas.md §2): an id (or address, at
    /// or before its permanently-recorded deletion ceiling) tombstoned by an
    /// earlier verified kind:5 is `Refused(Tombstoned)`, never stored.
    /// Otherwise run replaceable/addressable supersession (unchanged M1
    /// semantics). A kind:5 event is stored like any other regular event
    /// and, in the same call, drops every currently-held target it names
    /// whose author matches its own (NIP-09 author-only, enforced
    /// structurally) — see `Kind5Processed`.
    ///
    /// Fallible (issue #122): the ingest door runs on every relay EVENT
    /// frame, so a realistic persistence failure (disk full, I/O error) must
    /// return `Err(PersistenceError)` rather than panic the embedding app.
    /// The redb backend propagates the real redb error; `MemoryStore` never
    /// actually returns `Err` (no I/O). Serde/logic invariant violations
    /// (a corrupt stored row) remain `.expect()`-on-invariant, matching the
    /// durable-write doors' established convention.
    fn insert(
        &mut self,
        event: Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError>;

    /// Insert a relay-delivery batch in input order. Backends may override
    /// this to amortize durable transaction cost while preserving the exact
    /// per-event governed semantics and outcomes of repeated [`Self::insert`]
    /// calls. The default keeps non-transactional backends source-compatible.
    fn insert_batch(
        &mut self,
        events: Vec<(Event, RelayObserved)>,
    ) -> Result<Vec<InsertOutcome>, PersistenceError> {
        events
            .into_iter()
            .map(|(event, from)| self.insert(event, from))
            .collect()
    }

    /// Query current winners only (never a superseded/stale event), matched
    /// via `nostr::Filter::match_event`, each with its provenance attached.
    /// Fallible for the same reason as [`EventStore::insert`] (issue #122):
    /// a read-path I/O error surfaces as `Err` instead of panicking.
    ///
    /// `filter.limit` is NOT consulted by this LOCAL read path (#124): every
    /// currently-matching row is returned, in no particular order (neither
    /// backend orders its internal candidates by `created_at` — both are
    /// effectively id-keyed), regardless of `limit`. This is DELIBERATE, not
    /// an oversight — honoring `limit` locally requires a `created_at`-desc
    /// ordering + truncation, and choosing that ordering is an owner-
    /// reserved decision (issue #9's app-defined-sort-vs-closed-`OrderKey`
    /// fork, deferred to the Collection Tier-A gate), not something to
    /// settle as a side effect of this fix. Contrast with the WIRE path:
    /// `nmp_grammar::ConcreteFilter::to_nostr` DOES lower `limit` into this
    /// very `filter` before it ever reaches a relay, so a well-behaved
    /// relay caps what it SENDS you — a genuine, honored guarantee. But
    /// that guarantee governs the wire only; it says nothing about what a
    /// LATER local-only call to THIS method returns once the cache holds
    /// more than `limit` matching rows (reconnect replay, multiple relays
    /// each independently capped, etc.) — this method's own answer is
    /// uncapped regardless. Both backends are cross-checked for this exact
    /// contract (`store_contract.rs`); when #9 resolves, whoever implements
    /// ordered/truncated local reads updates that test, not just adds one.
    ///
    /// The app never sees this uncapped answer directly, though: the handle
    /// PROJECTION (`EngineCore::rows_and_evidence_for`, #124 via #139) caps the
    /// app-facing row set to the `limit` most recent by `created_at`
    /// (`EventId`-tiebroken). Persistent stores may use the separate
    /// [`EventStore::query_newest`] door to pre-bound each root atom before
    /// that final merged cap. That is NIP-01 limit-recency SELECTION — WHICH
    /// rows survive — not a display ordering: the app receives an unordered,
    /// `EventId`-keyed `RowDelta` stream and sorts it itself, so #9's
    /// display-sort fork stays open and the two compose. This store door
    /// deliberately stays uncapped so unlimited reactive recompute and
    /// negentropy still see every match. A `Derived` node carrying an explicit
    /// limit uses [`EventStore::query_newest`] instead: its projection is
    /// defined over the selected newest `N`, not over the complete history.
    fn query(&self, filter: &Filter) -> Result<Vec<StoredEvent>, PersistenceError>;

    /// Return at most `limit` current matches in NIP-01 newest-first
    /// selection order: `created_at` descending, then event id ascending.
    ///
    /// This is a distinct door from [`EventStore::query`], whose deliberately
    /// complete result is required by unlimited reactive recompute and
    /// negentropy. Handle root projections and explicitly limited `Derived`
    /// nodes use this bounded door. The default implementation preserves
    /// backend correctness by sorting the complete answer; persistent backends
    /// may override it with an ordered index scan that stops as soon as
    /// `limit` accepted rows have been found.
    fn query_newest(
        &self,
        filter: &Filter,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        let mut rows = self.query(filter)?;
        rows.sort_by(|a, b| {
            b.event
                .created_at
                .cmp(&a.event.created_at)
                .then_with(|| a.event.id.cmp(&b.event.id))
        });
        rows.truncate(limit);
        Ok(rows)
    }

    /// Return only the canonical ids from [`EventStore::query_newest`].
    ///
    /// Consumers that need selection identity but not event payloads use this
    /// door so persistent backends can project ids from ordered indexes
    /// without allocating owned content. The default preserves correctness
    /// for backends without a projected read path.
    fn query_newest_ids(
        &self,
        filter: &Filter,
        limit: usize,
    ) -> Result<Vec<EventId>, PersistenceError> {
        Ok(self
            .query_newest(filter, limit)?
            .into_iter()
            .map(|row| row.event.id)
            .collect())
    }

    /// Return the first `limit` canonical newest rows visible under a pin on
    /// `pinned` — [`Provenance::visible_under_pin`] is the one rule that
    /// decides which those are.
    ///
    /// This is the store-side projection required by a Strict pinned cache:
    /// the bound applies *after* visibility, never before it. Filtering an
    /// already-limited agnostic page can under-fill the result even when
    /// older visible rows exist. Persistent backends should test visibility
    /// while walking their ordered index and stop only after `limit` visible
    /// rows have been accepted.
    fn query_newest_under_pin(
        &self,
        filter: &Filter,
        pinned: &BTreeSet<RelayUrl>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut rows = self.query(filter)?;
        rows.retain(|row| row.provenance.visible_under_pin(pinned));
        rows.sort_by(|a, b| {
            b.event
                .created_at
                .cmp(&a.event.created_at)
                .then_with(|| a.event.id.cmp(&b.event.id))
        });
        rows.truncate(limit);
        Ok(rows)
    }

    /// Return at most `limit` current matches strictly after `before` in the
    /// canonical newest-first order used by [`EventStore::query_newest`].
    ///
    /// The exact exclusive predicate is:
    /// `created_at < before.created_at ||
    /// (created_at == before.created_at && id > before.event_id)`.
    /// This predicate intersects the filter's ordinary inclusive time window;
    /// it never rewrites that window or turns a cursor into relay acquisition
    /// authority. The default implementation is the `MemoryStore` oracle;
    /// persistent backends override it with an exact ordered-index range.
    fn query_newest_before(
        &self,
        filter: &Filter,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut rows = self.query(filter)?;
        rows.retain(|row| before.admits(&row.event));
        rows.sort_by(|a, b| {
            b.event
                .created_at
                .cmp(&a.event.created_at)
                .then_with(|| a.event.id.cmp(&b.event.id))
        });
        rows.truncate(limit);
        Ok(rows)
    }

    /// Pinned counterpart of [`EventStore::query_newest_before`]. The cursor
    /// remains exact and exclusive, while `limit` counts only rows visible
    /// under a pin on `pinned` ([`Provenance::visible_under_pin`]).
    fn query_newest_before_under_pin(
        &self,
        filter: &Filter,
        pinned: &BTreeSet<RelayUrl>,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut rows = self.query(filter)?;
        rows.retain(|row| before.admits(&row.event) && row.provenance.visible_under_pin(pinned));
        rows.sort_by(|a, b| {
            b.event
                .created_at
                .cmp(&a.event.created_at)
                .then_with(|| a.event.id.cmp(&b.event.id))
        });
        rows.truncate(limit);
        Ok(rows)
    }

    /// Return one canonical newest-first page from the UNION of `filters`,
    /// strictly after `before` in that order.
    ///
    /// A row matching more than one filter appears once. The global `limit`
    /// applies only after that de-duplication and merge, so callers can repair
    /// one bounded projection with one logical store read even when its
    /// resolved selection has multiple concrete roots. Persistent backends
    /// may evaluate each root with an ordered bounded scan: no row ranked
    /// below the first `limit` matches of its own root can enter the global
    /// first `limit` of the union.
    /// This remains selection-only; callers own presentation ordering.
    fn query_newest_before_any(
        &self,
        filters: &[Filter],
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        if limit == 0 || filters.is_empty() {
            return Ok(Vec::new());
        }
        let mut by_id = BTreeMap::new();
        for filter in filters {
            for row in self.query(filter)? {
                if before.admits(&row.event) {
                    by_id.entry(row.event.id).or_insert(row);
                }
            }
        }
        let mut rows: Vec<_> = by_id.into_values().collect();
        rows.sort_by(|a, b| {
            b.event
                .created_at
                .cmp(&a.event.created_at)
                .then_with(|| a.event.id.cmp(&b.event.id))
        });
        rows.truncate(limit);
        Ok(rows)
    }

    /// Pinned counterpart of [`EventStore::query_newest_before_any`]. The
    /// page bound counts only de-duplicated union rows visible under a pin
    /// on `pinned` ([`Provenance::visible_under_pin`]).
    fn query_newest_before_any_under_pin(
        &self,
        filters: &[Filter],
        pinned: &BTreeSet<RelayUrl>,
        before: EventCursor,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, PersistenceError> {
        if limit == 0 || filters.is_empty() {
            return Ok(Vec::new());
        }
        let mut by_id = BTreeMap::new();
        for filter in filters {
            for row in self.query(filter)? {
                if before.admits(&row.event) && row.provenance.visible_under_pin(pinned) {
                    by_id.entry(row.event.id).or_insert(row);
                }
            }
        }
        let mut rows: Vec<_> = by_id.into_values().collect();
        rows.sort_by(|a, b| {
            b.event
                .created_at
                .cmp(&a.event.created_at)
                .then_with(|| a.event.id.cmp(&b.event.id))
        });
        rows.truncate(limit);
        Ok(rows)
    }

    /// Remove `id` from the store — clearing both the id index and, if `id`
    /// is the current replaceable/addressable winner for its address, the
    /// address index too — and hand back the removed row whole, or `None`
    /// if `id` was not held. Engine-facing only (kind:5 processing,
    /// optimistic-write rejection); never a general delete API.
    fn remove(
        &mut self,
        id: EventId,
        reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError>;

    /// Drain every row whose NIP-40 `expiration` is `<= now`, removing each
    /// one (through the same [`EventStore::remove`] door) and returning the
    /// full rows. Index-backed (retraction-and-negative-deltas.md §3.1): a
    /// persistent `(expiry_ts -> {id})` index is maintained on every insert
    /// and every removal, so this drains in `O(log n + due)`, not a full
    /// scan.
    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError>;

    /// The earliest NIP-40 `expiration` deadline among currently stored
    /// rows, or `Ok(None)` if nothing carries one. Index-backed: peeks the
    /// minimum of the same persistent expiration index `expire_due` drains.
    ///
    /// Fallible for the same reason every other read door is (#122/#763): a
    /// backend read can fail for reasons that are not a bug in the caller —
    /// a disk error, a latched handle, a poisoned lock — and on an embedded
    /// host a panic here takes the whole application down. `Ok(None)` is
    /// honest absence and NOTHING else; a read that could not answer is
    /// `Err`.
    fn next_expiration(&self) -> Result<Option<Timestamp>, PersistenceError>;

    /// Atomically record every coverage claim earned by one completed
    /// request. Each tuple is `(atom, relay, proven interval)`. The coverage
    /// identity is the full [`ContextualAtom`], never a bare
    /// `ConcreteFilter`; the caller that owns request attribution supplies
    /// the complete batch. A successful return makes every merged claim
    /// visible, while an error may make none or the entire batch visible but
    /// never a prefix. Merge-only: no public lowering path exists outside
    /// `gc`.
    fn record_coverage(
        &mut self,
        claims: &[(ContextualAtom, RelayUrl, CoverageInterval)],
    ) -> Result<(), PersistenceError>;

    /// The proven interval for `key` at `relay`, or `Ok(None)` if no row
    /// exists. `Ok(None)` means this relay has no persisted interval for
    /// this key; it makes no wider claim.
    ///
    /// Fallible for the same reason [`EventStore::next_expiration`] is
    /// (#122/#763). The distinction is load-bearing here rather than merely
    /// tidy: "no coverage is proven" drives a refetch, while "the store
    /// could not be read" must not be answered as absent coverage, or a
    /// corrupt/unreadable watermark reads as an honest cache miss.
    fn get_coverage(
        &self,
        key: CoverageKey,
        relay: &RelayUrl,
    ) -> Result<Option<CoverageInterval>, PersistenceError>;

    /// Apply an EXPLICIT durable-retention policy by running claim-based GC
    /// (ruling §5): evicts every regular
    /// (non-replaceable, non-addressable) event matched by NO claim in
    /// `claims`. A claimed event, and every replaceable/addressable current
    /// winner, are ALWAYS retained — winners are never GC candidates at all,
    /// regardless of `claims`. When an evicted event falls inside a coverage
    /// row's proven interval and that row's retained shape matches it, the
    /// row is shrunk (or deleted, if the shrink empties it) in the same step
    /// — a watermark must never claim coverage of data no longer held.
    ///
    /// GC exclusion for open intents (Fable checkpoint R5): a row with
    /// local provenance still in `SigState::Pending` is NEVER a GC
    /// candidate, regardless of `claims` — structurally the same
    /// unconditional retention already given to replaceable/addressable
    /// winners, so an unsigned pending row can never be evicted before it
    /// ever signs. Once `promote_signed` flips it to `Signed`, it is an
    /// ordinary event again, GC-able like any other under `claims`.
    ///
    /// This is never an ordinary startup, query, shutdown, or implicit
    /// memory-pressure maintenance step. The production engine does not call
    /// this door: verified durable rows are retained by default. A host that
    /// deliberately adopts a quota, disk-pressure, or user-selected retention
    /// policy must make that policy inspectable and invoke this destructive
    /// door explicitly. Query/result/delivery bounds limit resident work; they
    /// are not permission to call `gc` or delete durable history.
    ///
    /// This contract does not promise infinite disk. It makes the transition
    /// from retained history to policy-evicted history explicit, reportable,
    /// and coverage-safe.
    fn gc(&mut self, claims: &GcRetentionSet) -> Result<GcReport, PersistenceError>;

    /// Accept a durably-owned local write intent (issues #2/#3): runs the
    /// SAME tombstone-refusal and replaceable/addressable supersession
    /// rules `insert` runs against `accept.frozen`, but stamps
    /// `Provenance::local_origin` instead of a `RelayObserved`, and commits
    /// the resulting row together with `accept`'s full journal payload
    /// (`PUBLISH_QUEUE_INTENTS` + `PUBLISH_QUEUE_DISPLACED`, if a predecessor was
    /// evicted) in ONE transaction (Fable checkpoint R7) — a crash mid-call
    /// leaves either nothing recoverable or a fully `recover_publish_queue`-able
    /// `Accepted`. `Refused` writes nothing at all (R3). A locally-composed
    /// kind:5 draft additionally runs the identical author-verified
    /// tombstone-write processing `insert` runs for a relay-observed
    /// kind:5, in the SAME transaction (architecture review correction:
    /// issue #2's immediate-delete promise extends to local compositions,
    /// not only the relay echo) — see `AcceptOutcome::Kind5Processed`.
    ///
    /// Fallible (architecture review correction,
    /// `docs/design/durable-write-signing-and-retry.md` §1: "if that
    /// transaction fails, the caller receives an acceptance error and no
    /// pending row becomes visible"): a realistic persistence failure
    /// (disk full, I/O error) returns `Err` rather than panicking the
    /// embedding app. As of issue #122 the ingest/read doors above
    /// (`insert`/`query`/`remove`/`expire_due`/`record_coverage`/`gc`) are
    /// fallible on the same footing; only serde/logic invariant violations
    /// (a corrupt persisted row) remain `.expect()`-on-invariant by design.
    /// `MemoryStore` never actually returns `Err` (no I/O).
    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError>;

    /// Swap the sentinel signature on `intent_id`'s frozen body for
    /// `verified`'s real one and flip the canonical
    /// `SigState`/`IntentSigState` to
    /// `Signed`, in the SAME transaction that durably drops the intent's
    /// own `PUBLISH_QUEUE_DISPLACED` stash (R6) and updates its retained receipt.
    /// Keyed by `IntentId`, NOT the frozen event's id (architecture review
    /// correction — load-bearing): the intent's `PUBLISH_QUEUE_INTENTS.frozen_json`
    /// is the durable source of truth for its body regardless of whether a
    /// live `EVENTS` row currently exists for it. Three cases, uniformly:
    /// (a) a live row's owner set CONTAINS `intent_id` (issue #2, team-lead
    /// decision: ownership is a SET — an exact `Duplicate` is a CO-OWNER
    /// of the SAME row, not a second row of its own; see `LocalOrigin`'s
    /// doc) — mutate it in place (same id — a NIP-01 id never depends on
    /// `sig` — so this is a value update, not a remove/re-add) — refused
    /// (`NotFound`) if the row's `SigState` is ALREADY `Signed`, even by a
    /// different co-owner, so a later distinct owner's promotion can never
    /// overwrite the one real signature with a second one; (b) no live
    /// row, but `intent_id` is a member of some OTHER intent's
    /// `PUBLISH_QUEUE_DISPLACED` stash entry's owner set (it was superseded by a
    /// later local edit before it could sign) — sync the real signature
    /// into that stash entry too (same already-`Signed` refusal applies),
    /// so a future restore of it never resurrects a stale sentinel copy;
    /// (c) neither (the intent was `Stale`/`Duplicate` at acceptance with
    /// no shared row, or its row was since superseded by a RELAY-observed
    /// event, kind:5-deleted, or NIP-40-expired) — mutate only the durable
    /// `PUBLISH_QUEUE_INTENTS`/`PUBLISH_QUEUE_RECEIPTS` journal copies; the resulting
    /// signed bytes are still returned so the engine can publish them even
    /// though this intent does not (or no longer) wins any local address.
    /// [`VerifiedSignature`] is the whole precondition, typed (#768): it
    /// cannot be built without one successful `nostr::Event::verify`, and
    /// this door refuses — [`PersistenceFault::Invariant`], before any
    /// mutation of any table — unless [`VerifiedSignature::event_id`]
    /// equals the intent's own durable frozen id. A signature that is
    /// perfectly valid for a DIFFERENT event is therefore refused here, not
    /// promoted. No implementation re-verifies: verification happened once,
    /// on the caller's side, to produce the evidence (#387). Fallible for
    /// the same reason `accept_write` is.
    fn promote_signed(
        &mut self,
        intent_id: IntentId,
        verified: VerifiedSignature,
    ) -> Result<PromoteOutcome, PersistenceError>;

    /// Pre-signature compensation only (retraction doc §4.2's "Promotion
    /// correction": once `promote_signed` has run, relay ACK/reject/timeout
    /// is receipt-only and NEVER reaches this door — a `Signed` intent
    /// answers `NotFound` here). Keyed by `IntentId` (same architecture
    /// review correction as `promote_signed`, same three cases, same
    /// ownership-SET model): (a) a live row's owner set CONTAINS
    /// `intent_id` — remove `intent_id` from that set; the row is only
    /// actually `remove(id, Rejected)`-ed (no tombstone) once the set is
    /// EMPTY, `SigState` is still `Pending`, AND no relay has
    /// independently confirmed it (`Provenance::seen` empty) — an exact
    /// `Duplicate`'s still-open obligation, an already-`Signed` state some
    /// OTHER co-owner committed, or independent relay provenance, all
    /// survive this one intent's cancellation (see `LocalOrigin`'s doc);
    /// if actually removed, this intent's durably-stashed `displaced`
    /// predecessor (if any) is then re-`insert`ed through the same one
    /// door — it wins its address back by ordinary supersession, never an
    /// un-supersede operation; (b) no live row, but `intent_id` is a
    /// member of some OTHER intent's `PUBLISH_QUEUE_DISPLACED` stash entry's
    /// owner set — same conditional removal, applied to that stash slot's
    /// owner set instead; (c) neither — nothing to remove or restore in
    /// `EVENTS`. In every case, this intent's own `PUBLISH_QUEUE_INTENTS`/
    /// `PUBLISH_QUEUE_DISPLACED` rows are deleted and its retained receipt
    /// updated to `Compensated`. Fallible for the same reason
    /// `accept_write` is.
    fn compensate_write(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.compensate_write_with_state(intent_id, CompensationReason::Failure)
    }

    /// The explicit-cancellation form of [`Self::compensate_write`]. It has
    /// identical atomic row/predecessor/lane semantics, but persists
    /// [`ReceiptState::Cancelled`] so reattachment can distinguish deliberate
    /// cancellation from a terminal signer/protocol failure.
    fn cancel_write(&mut self, intent_id: IntentId) -> Result<CompensateOutcome, PersistenceError> {
        self.compensate_write_with_state(intent_id, CompensationReason::ExplicitCancellation)
    }

    /// Read every retained receipt back out, newest id last (#1039).
    ///
    /// The enumeration half of the app's outbox door. Retained receipts
    /// accumulate without bound (#46 is NOT closed by this); making the
    /// growth readable is precisely the point.
    fn enumerate_publish_queue_receipts(
        &self,
    ) -> Result<Vec<PublishQueueReceipt>, PersistenceError>;

    /// Forget one retained receipt and every piece of evidence keyed to it
    /// (#1039). The removal half of the app's outbox door, and a real
    /// TERMINATION path: a write parked forever on a missing signer, and a
    /// permanently-failed refused entry, end no other way.
    ///
    /// Refuses with [`RemoveQueueEntryOutcome::StillOpen`] while the receipt
    /// still owns an open `PUBLISH_QUEUE_INTENTS` row — that write is
    /// cancelled, not removed.
    fn remove_publish_queue_entry(
        &mut self,
        receipt_id: u64,
    ) -> Result<RemoveQueueEntryOutcome, PersistenceError>;

    /// Backend implementation for the two typed pre-signature compensation
    /// outcomes. Callers use [`Self::compensate_write`] or
    /// [`Self::cancel_write`], never this shared atomic door directly.
    #[doc(hidden)]
    fn compensate_write_with_state(
        &mut self,
        intent_id: IntentId,
        reason: CompensationReason,
    ) -> Result<CompensateOutcome, PersistenceError>;

    /// Read every still-open intent back out of the durable journal on
    /// boot (issue #3 §2.3). Read-only: the pending rows themselves are
    /// already live in the store (committed at `accept_write` time) — this
    /// returns only the journal metadata `nmp-engine` needs to rebuild its
    /// in-memory write-delivery bookkeeping. `MemoryStore` always returns
    /// empty (Fable checkpoint Q4: crash-safety is a `RedbStore`-only
    /// backend property, not a contract `EventStore` itself promises).
    ///
    /// Fallible (#790). This used to return a bare `Vec`, which left the
    /// backend nothing to do with a journal row that will not decode except
    /// panic the embedding host at boot — the one moment the host is least
    /// able to survive it. `Ok(vec![])` and `Err(..)` are different facts and
    /// must stay distinguishable: the first says "no durable obligation is
    /// open", the second says "the durable obligation set is unreadable".
    /// A caller must never collapse the second into the first, and this door
    /// never returns a partial prefix — an undecodable row fails the whole
    /// call rather than silently shortening the obligation set.
    fn recover_publish_queue(&self) -> Result<Vec<PublishQueueIntent>, PersistenceError>;

    /// Look up `receipt_id`'s durably-RETAINED record — independent of
    /// whether its intent's `PUBLISH_QUEUE_INTENTS` open-work row still exists
    /// (architecture review correction: separates "recoverable open work"
    /// from "receipt identity/state", so a terminal receipt stays
    /// reattachable — issue #3's "receipts remain... reattachable" —
    /// rather than disappearing the moment its open-work row is cleaned
    /// up). Unlike `recover_publish_queue`, this is an ordinary retained-data
    /// lookup, not a boot-only replay: `MemoryStore` answers it faithfully
    /// for the life of the process (no Q4 "always empty" carve-out here —
    /// that carve-out is specifically about surviving a REAL crash, which
    /// this door never claims to do for a volatile backend).
    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<PublishQueueReceipt>, PersistenceError>;

    /// #591: resolve a caller's [`AcceptWrite::correlation`] token to the
    /// receipt id it was journaled under, if any. `Ok(None)` means the
    /// token has never been accepted (or this store never received it) --
    /// distinct from a persistence failure. `accept_write` uses this same
    /// mapping internally (checked inside its own transaction) to decide
    /// whether a token is a first sighting; the engine's
    /// `reattach_by_correlation` lookup door uses it directly to translate
    /// a token into an ordinary [`Self::reattach_receipt`] call. Retained
    /// forever, exactly like `PUBLISH_QUEUE_RECEIPTS` -- there is no removal door.
    fn lookup_correlation(&self, token: &str) -> Result<Option<u64>, PersistenceError>;

    /// Append the next canonical resolved-route revision for an open intent.
    /// This must commit before any attempt starts or wire publication for a
    /// relay in the revision.
    fn record_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<PublishQueueRouteRevision, PersistenceError>;

    /// Recover every resolved-route revision in ascending ordinal order.
    fn recover_route_revisions(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueRouteRevision>, PersistenceError>;

    /// Read all retained attempt facts for one intent in stable key order.
    fn recover_attempts(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueAttempt>, PersistenceError>;

    /// Idempotently seed every missing lane from bounded route/attempt
    /// ranges. Existing cursors are validated and retained.
    fn bootstrap_publish_queue_lanes(
        &mut self,
        _intent_id: IntentId,
    ) -> Result<Vec<PublishQueueLane>, PersistenceError> {
        Err(PersistenceError::invariant("delivery lanes unsupported"))
    }

    fn recover_publish_queue_lanes(
        &self,
        _intent_id: IntentId,
    ) -> Result<Vec<PublishQueueLane>, PersistenceError> {
        Err(PersistenceError::invariant("delivery lanes unsupported"))
    }

    /// Read at most `limit` due rows in stable `(time,intent,relay)` order.
    fn due_publish_queue_deadlines(
        &self,
        _now: Timestamp,
        _limit: usize,
    ) -> Result<Vec<PublishQueueDeadline>, PersistenceError> {
        Err(PersistenceError::invariant(
            "delivery deadlines unsupported",
        ))
    }

    fn next_publish_queue_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        Err(PersistenceError::invariant(
            "delivery deadlines unsupported",
        ))
    }

    fn set_lane_waiting(
        &mut self,
        _key: &PublishQueueLaneKey,
        _expected_revision: u64,
        _auth: bool,
    ) -> Result<PublishQueueLane, PersistenceError> {
        Err(PersistenceError::invariant("delivery lanes unsupported"))
    }

    fn set_lane_eligible(
        &mut self,
        _key: &PublishQueueLaneKey,
        _expected_revision: u64,
        _since: Timestamp,
    ) -> Result<PublishQueueLane, PersistenceError> {
        Err(PersistenceError::invariant("delivery lanes unsupported"))
    }

    fn set_lane_transient(
        &mut self,
        _key: &PublishQueueLaneKey,
        _expected_revision: u64,
        _ordinal: u64,
        _eligible_at: Timestamp,
        _cause: PublishQueueTransientCause,
        _raw_reason: Option<String>,
    ) -> Result<PublishQueueLane, PersistenceError> {
        Err(PersistenceError::invariant("delivery lanes unsupported"))
    }

    /// End the current ordinal as a nonterminal wait with no deadline.
    /// The attempt detail and waiting cursor advance atomically, so restart
    /// cannot mistake an AUTH/offline wait for a live ambiguous send.
    #[allow(clippy::too_many_arguments)]
    fn suspend_lane_attempt(
        &mut self,
        _key: &PublishQueueLaneKey,
        _expected_revision: u64,
        _ordinal: u64,
        _at: Timestamp,
        _cause: PublishQueueTransientCause,
        _raw_reason: Option<String>,
        _auth: bool,
    ) -> Result<PublishQueueLane, PersistenceError> {
        Err(PersistenceError::invariant("delivery lanes unsupported"))
    }

    /// Atomically append new immutable v1 Started evidence, additive details,
    /// and advance an eligible cursor to awaiting handoff.
    fn start_lane_attempt(
        &mut self,
        _key: &PublishQueueLaneKey,
        _expected_revision: u64,
        _event: Event,
        _started_at: Timestamp,
    ) -> Result<(PublishQueueAttempt, PublishQueueLane), PersistenceError> {
        Err(PersistenceError::invariant("delivery lanes unsupported"))
    }

    /// Atomically retain handoff evidence and apply the engine-selected next
    /// fact, maintaining the typed deadline index in the same commit.
    fn record_lane_handoff(
        &mut self,
        _key: &PublishQueueLaneKey,
        _expected_revision: u64,
        _ordinal: u64,
        _detail: PublishQueueAttemptHandoff,
        _next: PublishQueuePostHandoffState,
    ) -> Result<PublishQueueLane, PersistenceError> {
        Err(PersistenceError::invariant("delivery lanes unsupported"))
    }

    /// Make the current attempt terminal without rewriting its immutable v1
    /// Started row. Exact ordinal + lane revision reject late ACKs against a
    /// newer attempt; detail, cursor, and deadline removal share one commit.
    fn finish_lane_attempt(
        &mut self,
        _key: &PublishQueueLaneKey,
        _expected_revision: u64,
        _ordinal: u64,
        _outcome: PublishQueueAttemptOutcome,
        _finished_at: Timestamp,
    ) -> Result<PublishQueueLane, PersistenceError> {
        Err(PersistenceError::invariant("delivery lanes unsupported"))
    }

    /// Atomically finish an exact AUTH-waiting lane without fabricating an
    /// EVENT attempt. Exact lane revision is checked before idempotence, so a
    /// stale writer can never borrow success from a newer terminal fact.
    fn deny_lane_auth(
        &mut self,
        _key: &PublishQueueLaneKey,
        _expected_revision: u64,
        _denial: AuthDenial,
    ) -> Result<PublishQueueLane, PersistenceError> {
        Err(PersistenceError::invariant("delivery lanes unsupported"))
    }

    fn recover_attempt_details(
        &self,
        _intent_id: IntentId,
    ) -> Result<Vec<PublishQueueAttemptDetails>, PersistenceError> {
        Err(PersistenceError::invariant(
            "delivery attempt details unsupported",
        ))
    }

    /// Delete an intent's bounded open-work rows when it owns NO lanes at all.
    ///
    /// The exact structural complement of [`Self::close_terminal_intent`],
    /// which requires a NON-EMPTY all-terminal lane set. Zero lanes is a fact
    /// this crate can check for itself, so neither door asks the store to
    /// guess at routing policy: the engine calls this one only when its own
    /// resolution reported knowledge exhausted with zero destinations, and
    /// the store still refuses if any lane exists.
    ///
    /// Without it a write that resolved to nowhere kept its open-work row
    /// forever — unremovable (the removal door refuses an open intent),
    /// uncancellable once signed, and replayed on every boot. That is the
    /// FIRST-RUN path now that a fresh install with no reachable relay list
    /// terminates as `NoDestination`, so it is a leak on the most common
    /// path rather than an edge case.
    ///
    /// Receipts stay retained and reattachable, exactly as
    /// [`Self::close_terminal_intent`] leaves them.
    fn close_unroutable_intent(
        &mut self,
        _intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        Err(PersistenceError::invariant("delivery lanes unsupported"))
    }

    /// Delete bounded open-work rows only after a non-empty lane set is all
    /// terminal. Receipts and all route/attempt/detail evidence are retained.
    fn close_terminal_intent(
        &mut self,
        _intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        Err(PersistenceError::invariant("delivery lanes unsupported"))
    }

    /// Take custody of a write the acceptance door REFUSED, as one
    /// permanently-failed queue entry.
    ///
    /// `accept_write` answering [`AcceptOutcome::Refused`] is the store
    /// working and saying no — a semantic answer, not a failure to write.
    /// The app must be able to read that answer back, so the refusal is
    /// recorded rather than thrown: THIS door writes just the
    /// `PUBLISH_QUEUE_RECEIPTS` row with `intent_id: None` (nothing backs it
    /// — no intent, no journal, no pending event row, no signer request, no
    /// relay write) and [`ReceiptState::Refused`] carrying `reason`
    /// verbatim, including a [`RefuseReason::ReplaceableBaseChanged`]'s two
    /// event ids.
    ///
    /// Terminal at birth. Custody is not viability: the entry exists so the
    /// app can see the failure and remove it
    /// ([`Self::remove_publish_queue_entry`]), never because anything will
    /// retry it.
    ///
    /// Returns the store-allocated receipt id — the same durable
    /// high-water-mark `accept_write` allocates from (architecture review
    /// correction: a caller-side receipt-id counter that resets on
    /// restart has the identical reuse hazard `IntentId` had, now that
    /// receipts are durably retained across restart). Fallible for the
    /// same reason `accept_write` is: recording a refusal needs the disk.
    fn accept_refused(
        &mut self,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
        reason: RefuseReason,
    ) -> Result<u64, PersistenceError>;
}
