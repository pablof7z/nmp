//! The publish-queue write vocabulary: what an app learns about a write it
//! handed to NMP, and the two errors that mean NMP never took it.
//!
//! **Publish takes custody.** `publish()` returning `Ok` is acceptance: the
//! write is durably recorded and whatever becomes of it is recorded with it.
//! Custody is **not** viability — a write can be in custody and already
//! permanently failed. Nothing here may be read as "it will eventually
//! publish."
//!
//! The call itself refuses in exactly two situations, and only these
//! ([`PublishError`]):
//!
//! 1. **NMP cannot write anything down** — the engine is draining, the
//!    receipt id space ran out (a queue entry *requires* a receipt id), or
//!    the disk failed under the acceptance transaction.
//! 2. **The instruction cannot resolve** — a caller-supplied signature that
//!    does not verify, an [`Identity::Active`](nmp_grammar::Identity::Active)
//!    write with no current account, an explicit identity contradicting a
//!    signed payload's own author, a kind the reducer owns. Nothing in this
//!    class is a fact about the WORLD: no relays, no signing provider available,
//!    and disk trouble all take custody and fail in the queue instead, where the app
//!    can see them. *"An instruction that cannot resolve is a refusal, not a
//!    parked hope"* (`nmp-grammar/src/write.rs`).
//!
//! Everything else — including a stale replaceable base — takes custody and
//! becomes a queue entry the app can read back.
//!
//! ## Nothing terminates on a clock; everything terminates on a fact
//!
//! An attempt ceiling (`nmp::EngineConfig::max_publish_attempts`)
//! counts OBSERVATIONS and is therefore legitimate: "we tried N times and it
//! failed N times." A time budget is not, because it converts ignorance into
//! a verdict. A write parked on an unresolved route or unavailable signing
//! provider has no cap of any kind — it ends when knowledge is exhausted,
//! when the configured provider becomes available, or when the app removes it.
//!
//! ## Optimistic publishing
//!
//! Local visibility never waits for settlement: an accepted write appears in
//! the app's own live query immediately. When a workflow genuinely needs the
//! relay answer, `nmp::ReceiptStream::result`
//! awaits the typed terminal and performs reduction/replay inside NMP rather
//! than making each app reimplement receipt semantics.

mod result;

use std::collections::BTreeSet;

use nostr::{EventId, PublicKey, RelayUrl, Timestamp};

use crate::core::ReceiptId;

pub use nmp_store::RefuseReason;
pub use result::{ReceiptResult, ReceiptResultError};

/// Enough failures at one relay to call it: roughly a day of the capped
/// 3s-doubling-to-300s backoff schedule, which is long enough that a relay
/// having a bad afternoon is not abandoned and short enough that a relay
/// that is simply gone stops holding an obligation open forever.
///
/// Defined here, beside the ceiling it is the default for, rather than in
/// `nmp::EngineConfig`: the queue is what counts attempts and refuses past the
/// ceiling, so the engine reads its own default rather than reaching up into
/// facade configuration for it. This matches the two sibling defaults —
/// `nmp_transport::DEFAULT_MAX_RELAYS` and
/// `nmp_runtime::DEFAULT_MAX_AUTH_CAPABILITIES` — which already live
/// with their enforcers.
pub const DEFAULT_MAX_PUBLISH_ATTEMPTS: u64 = 16;

/// Which exact AUTH actor refused a write session.
///
/// The distinction prevents a local policy or signer choice from being
/// mislabeled as a relay rejection. Four situations, four remediations:
/// [`Self::Policy`] is reversible by changing the app's own choice,
/// [`Self::Signer`] by unlocking the device, [`Self::Relay`] not at all —
/// and a relay that authenticated the identity and then refused this one
/// event is [`RelayState::Rejected`], which is a different repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDenialSource {
    Policy,
    Signer,
    Relay,
}

/// Why a lane will try again. AUTH-required is deliberately absent: waiting
/// for AUTH is [`RelayWaiting::NeedsAuth`], not a retryable EVENT outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryCause {
    Interrupted,
    AckTimeout,
    ConnectionLost,
    RelayRateLimited,
    RelayError,
}

/// The signing state of the WHOLE write — one signature, one author, one
/// answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigningState {
    /// No available signing provider answers for `pubkey` — the exact identity
    /// FROZEN at acceptance, never whoever happens to be current now. Re-armed
    /// only when the configured provider for THIS key becomes available, and
    /// re-emitted verbatim on restart replay.
    ///
    /// **No clock ever ends this.** A device whose provider is currently
    /// unavailable is not a device whose write failed; the app's own
    /// decision is the only other exit, and it is two calls:
    /// `Handle::cancel_write` ends the obligation and compensates the
    /// optimistic row the write promised, then
    /// `Handle::remove_publish_queue_entry` forgets the terminal receipt it
    /// leaves behind.
    ///
    /// This is the state a person has to be told about, and [`Self::InFlight`]
    /// is the one it must never be confused with.
    AwaitingSigner { pubkey: PublicKey },
    /// A signer for `pubkey` HAS the request and has not answered yet — the
    /// ordinary state of every healthy write between acceptance and
    /// signature promotion.
    ///
    /// Transient and normal, and it ends on a fact rather than a clock: the
    /// signer answers ([`Self::Signed`] or [`Self::Refused`]), or the signer
    /// becomes unavailable and the write falls back to
    /// [`Self::AwaitingSigner`]. Nothing here is a reason to trouble a user.
    ///
    /// Collapsing this onto [`Self::AwaitingSigner`] (#1261) makes every
    /// healthy write read as parked, and leaves the genuinely parked write —
    /// the one whose only other exit is the app cancelling it and removing
    /// its entry — impossible to pick out.
    InFlight { pubkey: PublicKey },
    /// A signature exists and the write has an id.
    Signed { event_id: EventId },
    /// The signer answered and said no. Terminal for the whole write: there
    /// is one signature to obtain and it was refused.
    Refused { reason: String },
}

/// Why a relay lane is not attempting right now. Every arm is a fact about
/// the lane, and none of them is a deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayWaiting {
    /// The connection is unavailable. Offline time consumes no attempt
    /// ordinal, so being offline can never spend the ceiling.
    NotConnected,
    /// This relay requires AUTH before the lane may try again. AUTH-blocked
    /// time has no retry deadline and consumes no new attempt.
    NeedsAuth,
    /// The last attempt failed in a way that permits another one, and
    /// `cause`/`detail` say WHY.
    ///
    /// The pair is load-bearing rather than decoration (#1032): "we will try
    /// again" and "we will try again because the relay rate-limited us" are
    /// different messages and only the second one can be acted on. `attempt`
    /// is the persisted ordinal whose outcome established this eligibility;
    /// the next wire attempt, if one is made, receives a fresh ordinal.
    BackingOff {
        attempt: u64,
        eligible_at: Timestamp,
        cause: RetryCause,
        detail: Option<String>,
    },
    /// The lane is owned and nonterminal, but a durable fact about it could
    /// not be committed — the local disk is refusing writes. No wire EVENT
    /// was emitted.
    ///
    /// This is the only arm that is ALSO latched onto the queue entry
    /// ([`PublishQueueEntry::persistence_fault`]). A blockage that arose and
    /// resolved before the app looked would otherwise vanish, and an
    /// operator would lose the only signal that the disk is failing. It is
    /// emitted as a fact AND readable on the entry, and a later ack never
    /// overwrites the latch.
    ///
    /// `detail` carries the recovery difference the two old spellings
    /// encoded: whether the resolved relay URL itself survives a crash
    /// (an attempt-log stall) or does not (a route-revision stall).
    PersistenceStalled { detail: String },
}

/// What is true at ONE relay.
///
/// [`Self::Published`], [`Self::Rejected`], [`Self::AuthFailed`] and
/// [`Self::GaveUp`] are terminal for that relay; [`Self::Waiting`] and
/// [`Self::Sent`] are not. A write is settled when the destination set is
/// closed and every member is terminal — see [`WriteOutcome::Settled`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayState {
    Waiting(RelayWaiting),
    /// Transport proved socket write + flush for this persisted attempt.
    /// Not an ack, and not terminal: the relay has not answered yet.
    Sent {
        attempt: u64,
        written_at: Timestamp,
    },
    /// The relay acked this event.
    Published,
    /// The relay authenticated the identity and refused THIS EVENT. The
    /// repair is to the event.
    Rejected {
        reason: String,
    },
    /// The write could not be authenticated HERE. `source` names which actor
    /// refused, because the app's own decision not to authenticate must
    /// never be reported to a user as a relay refusing them. `pubkey` keeps
    /// two sessions on one relay URL distinguishable.
    AuthFailed {
        pubkey: PublicKey,
        source: AuthDenialSource,
        reason: String,
    },
    /// The attempt ceiling was reached at this relay. Terminal here and
    /// nowhere else: a four-relay publish where one relay is given up on and
    /// three published is a success with a footnote.
    GaveUp,
}

impl RelayState {
    /// Whether this relay will produce another fact. The one question a
    /// consumer used to re-derive by hand, in four disagreeing copies.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Published | Self::Rejected { .. } | Self::AuthFailed { .. } | Self::GaveUp => {
                true
            }
            Self::Waiting(_) | Self::Sent { .. } => false,
        }
    }
}

/// Why a write ended without going anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotSentReason {
    /// The app explicitly cancelled the obligation before signature
    /// promotion. Compensation committed atomically.
    Cancelled,
    /// The signer explicitly refused or failed the accepted signing request.
    /// Compensation proved that no EVENT bytes could have crossed the local
    /// transport handoff.
    SignerRefused,
    /// A newer accepted write won the same NIP-01 replaceable/addressable
    /// coordinate, and NMP proved the older bytes did not cross the local
    /// transport handoff. Not a failure — for an app renewing presence it is
    /// the steady state.
    Superseded,
}

/// The whole-write terminal. Exactly one of these ends every receipt stream,
/// so a stream can never end in silence and an app can always tell a
/// finished write from a dropped subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The destination set is CLOSED and every relay in it is terminal. What
    /// happened at each is the per-relay facts; this says only that no more
    /// are coming.
    Settled,
    /// Routing finished — knowledge is exhausted — and named zero relays.
    /// Terminal: there is nowhere to publish. Distinct from a route still
    /// resolving, which parks forever and is `complete: false` with an empty
    /// destination set.
    NoDestination,
    NotSent(NotSentReason),
    /// A newer replaceable write retired this obligation after its bytes may
    /// already have crossed the local transport handoff. NMP will not retry
    /// it, but does not falsely claim it was never sent.
    Superseded,
    /// The store answered the acceptance instruction with a semantic no. The
    /// write is in custody as a permanently-failed entry: one row, payload
    /// intact, readable and removable through the enumeration door.
    Refused(RefuseReason),
}

/// One fact about a write, delivered on its receipt stream.
///
/// The old vocabulary mixed facts about the whole write with facts about one
/// relay in a single flat enum, which is why "is this status terminal?" had
/// no answer: `Acked(relay)` closed a lane while the write continued. Here
/// the two live on different arms, and [`Self::Outcome`] is the only thing
/// that ends anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteFact {
    Signing(SigningState),
    Relay {
        /// Exact immutable event whose relay evidence this fact records.
        /// Stable semantic receipts can span several successor generations;
        /// the receipt id therefore cannot identify these bytes.
        event_id: EventId,
        relay: RelayUrl,
        state: RelayState,
    },
    /// The relays this write is INTENDED for, and whether resolution can
    /// still change its mind.
    ///
    /// The two axes are deliberately separate. `complete` flips on settled
    /// RESOLUTION — zero remaining unknowns — never on successful delivery.
    /// So `complete: true` with every relay unpublished is an ordinary state
    /// ("we know exactly where this goes; it has not gone yet"), and so is
    /// `complete: false` with some relays already published.
    ///
    /// This is the settlement DENOMINATOR: without the intended set and its
    /// closedness an app cannot tell "all lanes done" from "still learning",
    /// which is exactly the silence this vocabulary exists to remove.
    /// Re-emitted whenever resolution changes the picture.
    ///
    /// `complete: false` with an empty set is a write still learning where
    /// it goes; it parks indefinitely and NOTHING expires it.
    /// `complete: true` with an empty set is [`WriteOutcome::NoDestination`],
    /// which follows immediately.
    ///
    /// `awaiting_author_routes` is WHY resolution is still open, as keys
    /// rather than as a sentence: every public key whose author routes this
    /// write is still waiting on. A later positive route fact for any one of
    /// them is the only thing that can move the picture, so this set is both
    /// the reason and the list of repairs — an app can name the people it is
    /// waiting for, and an operator can see whether the wait is a missing
    /// discovery source or a user who has never published a relay list.
    /// Non-empty implies `complete: false`; a settled resolution has nothing
    /// left to wait on and always names nobody. The converse does NOT hold.
    /// An open picture that names nobody can be a write whose routing has not
    /// run because it is not signed yet — then [`Self::Signing`] says what
    /// holds it — or a signed Auto route whose canonical parent-provenance
    /// read hit a persistence fault. The latter is visible in engine
    /// diagnostics and retried after store recovery; naming an author here
    /// would invent a route lookup that is not actually outstanding.
    ///
    /// It is deliberately not a rendered string. "Still determining" and
    /// "nowhere to send" were once one English sentence, which no program
    /// could branch on (#1236); the branch is `complete`, and this set is the
    /// detail behind the open side of it.
    Destinations {
        relays: BTreeSet<RelayUrl>,
        complete: bool,
        awaiting_author_routes: BTreeSet<PublicKey>,
    },
    Outcome(WriteOutcome),
}

/// One write in the queue, as the app reads it back (#1039).
///
/// Enumerating the queue is how an app answers "what have I got outstanding,
/// and what went wrong with it" without having held a receipt stream open
/// since acceptance. Removal is the companion half and is not optional:
/// a write parked on an unavailable signing provider, and a permanently-failed entry, end
/// only by the app's own decision — cancel the parked one, then remove the
/// terminal receipt either leaves behind — so removal is a termination path
/// rather than housekeeping.
///
/// Superseded safety receipts are automatically age/count bounded. Other
/// terminal classes remain app-removable, and #46 continues to own their
/// general retention policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishQueueEntry {
    pub receipt_id: ReceiptId,
    /// The frozen event id — the write's identity from acceptance onward,
    /// unchanged by signing (a NIP-01 id never depends on `sig`).
    pub event_id: EventId,
    /// The identity frozen at acceptance. Never re-resolved.
    pub pubkey: PublicKey,
    pub accepted_at: Timestamp,
    pub signing: SigningState,
    /// The intended destination set and whether it is closed. Empty and open
    /// means routing is still learning.
    pub relays: BTreeSet<RelayUrl>,
    pub route_complete: bool,
    /// Per-relay state for every member of `relays` NMP has a fact about.
    pub relay_states: Vec<(RelayUrl, RelayState)>,
    /// `Some` once the whole write ended; `None` while it is in progress.
    pub outcome: Option<WriteOutcome>,
    /// LATCHED. Set the first time local persistence refused a durable fact
    /// for this write, and never cleared by a later success — an operator
    /// must not lose the only signal that the disk is failing because a
    /// relay acked afterwards.
    pub persistence_fault: Option<String>,
}

impl PublishQueueEntry {
    /// Whether this write will produce another fact.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.outcome.is_some()
    }
}

/// Typed failure from bounded publish-queue inspection (#903).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishQueueReadError {
    PersistenceFailed { reason: String },
    EngineClosed,
}

impl std::fmt::Display for PublishQueueReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PersistenceFailed { reason } => {
                write!(f, "could not inspect the publish queue: {reason}")
            }
            Self::EngineClosed => write!(f, "engine already shut down"),
        }
    }
}

impl std::error::Error for PublishQueueReadError {}

/// Why removing a queue entry did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveQueueEntryError {
    UnknownReceipt {
        receipt_id: ReceiptId,
    },
    /// The write's obligation is still open — nothing has ended it yet,
    /// whether it is signed with live lanes or parked on an unavailable
    /// signing provider. Cancel it first; removal is for the terminal receipt that
    /// leaves behind.
    StillActive {
        receipt_id: ReceiptId,
    },
    PersistenceFailed {
        receipt_id: ReceiptId,
        reason: String,
    },
    EngineClosed,
}

impl std::fmt::Display for RemoveQueueEntryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownReceipt { receipt_id } => {
                write!(f, "unknown receipt {}", receipt_id.0)
            }
            Self::StillActive { receipt_id } => write!(
                f,
                "receipt {} still owns open delivery work; cancel it first",
                receipt_id.0
            ),
            Self::PersistenceFailed { receipt_id, reason } => write!(
                f,
                "could not remove queue entry for receipt {}: {reason}",
                receipt_id.0
            ),
            Self::EngineClosed => write!(f, "engine already shut down"),
        }
    }
}

impl std::error::Error for RemoveQueueEntryError {}

/// The only successful result of explicit write cancellation. Keeping this
/// separate from the fact vocabulary makes every other receipt state
/// unrepresentable as a successful cancellation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelWriteOutcome {
    Cancelled,
}

/// Typed refusal from explicit pre-signature cancellation. Each terminal
/// state has its own construction path, so already-cancelled cannot be
/// represented as a refusal and accepted cannot masquerade as terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelWriteError {
    UnknownReceipt {
        receipt_id: ReceiptId,
    },
    AlreadySigned {
        receipt_id: ReceiptId,
        event_id: EventId,
    },
    AlreadyCompensated {
        receipt_id: ReceiptId,
    },
    AlreadySuperseded {
        receipt_id: ReceiptId,
    },
    /// The write was refused at acceptance and is already a permanently
    /// failed entry. There is nothing to cancel; remove it instead.
    AlreadyRefused {
        receipt_id: ReceiptId,
    },
    PersistenceFailed {
        receipt_id: ReceiptId,
        reason: String,
    },
    EngineClosed,
}

impl std::fmt::Display for CancelWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownReceipt { receipt_id } => {
                write!(f, "unknown receipt {}", receipt_id.0)
            }
            Self::AlreadySigned {
                receipt_id,
                event_id,
            } => write!(
                f,
                "receipt {} is already signed as {event_id}",
                receipt_id.0
            ),
            Self::AlreadyCompensated { receipt_id } => {
                write!(f, "receipt {} is already compensated", receipt_id.0)
            }
            Self::AlreadySuperseded { receipt_id } => {
                write!(
                    f,
                    "receipt {} was superseded by a newer write",
                    receipt_id.0
                )
            }
            Self::AlreadyRefused { receipt_id } => {
                write!(f, "receipt {} was refused at acceptance", receipt_id.0)
            }
            Self::PersistenceFailed { receipt_id, reason } => write!(
                f,
                "could not persist cancellation for receipt {}: {reason}",
                receipt_id.0
            ),
            Self::EngineClosed => write!(f, "engine already shut down"),
        }
    }
}

impl std::error::Error for CancelWriteError {}

/// What `Handle::publish` returns: an id correlating to the runtime-delivered
/// fact stream — never a `bool`/`()`.
pub struct Receipt {
    pub id: ReceiptId,
}
