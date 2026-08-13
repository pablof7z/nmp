// The write noun, in ergonomic Swift shape (M4 plan §9).

import NMPFFI

public enum AuthDenialSource: Sendable, Hashable {
    case policy
    case signer
    case relay

    init(_ ffi: FfiAuthDenialSource) {
        switch ffi {
        case .policy: self = .policy
        case .signer: self = .signer
        case .relay: self = .relay
        }
    }
}

public enum RetryCause: Sendable, Hashable {
    case interrupted
    case ackTimeout
    case connectionLost
    case relayRateLimited
    case relayError

    init(_ ffi: FfiRetryCause) {
        switch ffi {
        case .interrupted: self = .interrupted
        case .ackTimeout: self = .ackTimeout
        case .connectionLost: self = .connectionLost
        case .relayRateLimited: self = .relayRateLimited
        case .relayError: self = .relayError
        }
    }
}

/// `.explicit` is a general capability, not a protocol-module privilege: an
/// app offering "publish this event to relay: [user input]", a wiki module
/// publishing to the user's preferred wiki relays, and a user archiving
/// someone else's signed note to their own relay are all the same
/// primitive. It executes verbatim -- the relay directory is never
/// consulted, and nothing learned later widens it -- and an empty `relays`
/// is refused at the door.
// nmp-native:if nip65
///
/// `.auto` asks the selected outbox-routing capability to discover
/// author-write and recipient-read routes. An engine constructed without
/// outbox-routing indexers refuses it before durable acceptance.
// nmp-native:endif
public enum WriteRouting: Sendable, Hashable {
    // nmp-native:if nip65
    case auto
    // nmp-native:endif
    case explicit(relays: [String])

    func toFfi() -> FfiWriteRouting {
        switch self {
        // nmp-native:if nip65
        case .auto: return .auto
        // nmp-native:endif
        case let .explicit(relays): return .explicit(relays: relays)
        }
    }

    init(_ ffi: FfiWriteRouting) {
        switch ffi {
        // nmp-native:if nip65
        case .auto: self = .auto
        // nmp-native:endif
        case let .explicit(relays): self = .explicit(relays: relays)
        }
    }
}

/// The event payload of a write intent (`FfiWritePayload` mirror). VISION
/// P: signing and publishing are ORTHOGONAL stages -- `.event` describes an
/// event NMP stamps, freezes and signs itself. The kind is the one thing it
/// cannot invent, so the kind is the one thing it demands; the account it
/// publishes as comes from the write's identity (see
/// `current-account selection` and `WriteIntent.identity`), never
/// from the payload, and `createdAt` is stamped at acceptance unless you
/// state one -- state one and it is kept exactly.
///
/// `.signed` (#32, the M5 unlock) is a caller that already holds a
/// validly-signed event -- an external signer provider, or a verbatim
/// republish of somebody else's note to an archive relay -- and hands its
/// fields across as-is: the engine verifies then publishes it exactly as
/// given, never re-signing, mutating a tag, or recomputing an id.
public enum WritePayload: Sendable, Hashable {
    /// Everything you must say is `kind`. `tags`, `content` and `createdAt`
    /// default, and there is deliberately no `pubkey`, `id` or `sig`.
    case event(kind: UInt16, tags: [[String]] = [], content: String = "", createdAt: UInt64? = nil)
    case signed(
        id: String, pubkey: String, createdAt: UInt64, kind: UInt16, tags: [[String]],
        content: String, sig: String)

    func toFfi() -> FfiWritePayload {
        switch self {
        case .event(let kind, let tags, let content, let createdAt):
            return .event(
                builder: FfiEventBuilder(
                    kind: kind, tags: tags, content: content, createdAt: createdAt))
        case .signed(let id, let pubkey, let createdAt, let kind, let tags, let content, let sig):
            return .signed(
                id: id, pubkey: pubkey, createdAt: createdAt, kind: kind, tags: tags, content: content,
                sig: sig)
        }
    }

    init(_ ffi: FfiWritePayload) {
        switch ffi {
        case .event(let builder):
            self = .event(
                kind: builder.kind,
                tags: builder.tags,
                content: builder.content,
                createdAt: builder.createdAt
            )
        case .signed(let id, let pubkey, let createdAt, let kind, let tags, let content, let sig):
            self = .signed(
                id: id,
                pubkey: pubkey,
                createdAt: createdAt,
                kind: kind,
                tags: tags,
                content: content,
                sig: sig
            )
        }
    }
}

/// The identity one write publishes under (`FfiIdentity` mirror). Exactly
/// two words, and neither of them is an absence: `.active` is a positive
/// instruction ("whoever is the current account when this is accepted"),
/// which is why there is no third "unset" case here or anywhere else.
///
/// On an `.event` payload the identity SELECTS the author -- a builder
/// states none, so there is nothing for it to contradict. On a `.signed`
/// payload it may only RESTATE the author already frozen in the bytes:
/// naming that author changes nothing, naming anybody else is a
/// consent/author contradiction that surfaces as `WriteFact.failed` on
/// the receipt stream with no `.accepted` before it.
///
/// `.explicit`'s `pubkey` is 64-char HEX and nothing else. A bech32 `npub`
/// is refused however well-formed it is (`NMPError.invalidPublicKey`,
/// thrown synchronously from `publish`): bech32 is how something is shown
/// to a person or received from one, so an app that took an npub from a
/// paste box decodes it there -- with `decodeNostrEntity` -- and hands NMP
/// a key. Naming a pubkey with no configured signing provider is NOT an error:
/// the write parks as `.awaitingCapability` until that account's provider
/// becomes available.
/// Acceptance pins the resolved key either way, so a later
/// current-account selection cannot retarget the write.
public enum Identity: Sendable, Hashable {
    case active
    case explicit(pubkey: String)

    func toFfi() -> FfiIdentity {
        switch self {
        case .active: return .active
        case let .explicit(pubkey): return .explicit(pubkey: pubkey)
        }
    }

    init(_ ffi: FfiIdentity) {
        switch ffi {
        case .active: self = .active
        case let .explicit(pubkey): self = .explicit(pubkey: pubkey)
        }
    }
}

/// A caller's publish request (`FfiWriteIntent` mirror).
///
/// `identity` (#47) defaults to `.active` -- the overwhelming majority of
/// writes publish as the logged-in account, and saying so costs nothing.
/// `SigningState.awaitingSigner`'s associated `pubkey` (#47 Unit B) is
/// the exact frozen identity parked -- the key `.explicit` named, else the
/// account that was active at publish time -- never a different,
/// later-current account.
public struct WriteIntent: Sendable, Hashable {
    public var payload: WritePayload
    public var routing: WriteRouting
    public var identity: Identity
    /// Crash-safe client correlation token (#591). `nil` -- the default --
    /// opts this write out of correlation entirely. A non-`nil` token is
    /// validated by `nmp_grammar::CorrelationToken`'s `TryFrom<&str>` on the way across
    /// the boundary (non-empty, length-capped); a malformed token throws
    /// `NMPError.invalidCorrelationToken` synchronously from `publish`,
    /// before any engine call. A token that already resolves to a
    /// previously-accepted receipt reattaches that existing obligation
    /// instead of enqueuing a second write -- no body comparison against
    /// `payload`. See `NMPEngine.reattachReceipt(correlation:)` for the
    /// door that recovers a receipt after a crash that happened BEFORE the
    /// app could durably persist the id `publish` returned.
    public var correlation: String?

    public init(
        payload: WritePayload,
        routing: WriteRouting,
        identity: Identity = .active,
        correlation: String? = nil
    ) {
        self.payload = payload
        self.routing = routing
        self.identity = identity
        self.correlation = correlation
    }

    /// Reverse projection for protocol-owned FFI composers that return the
    /// ordinary write noun. Kept internal: apps construct `WriteIntent`
    /// directly or receive one from a typed protocol function.
    init(_ ffi: FfiWriteIntent) {
        payload = WritePayload(ffi.payload)
        routing = WriteRouting(ffi.routing)
        identity = Identity(ffi.identity)
        correlation = ffi.correlation
    }

    func toFfi() -> FfiWriteIntent {
        FfiWriteIntent(
            payload: payload.toFfi(),
            routing: routing.toFfi(),
            identity: identity.toFfi(),
            correlation: correlation
        )
    }
}

/// The signing state of the WHOLE write -- one signature, one author, one
/// answer.
public enum SigningState: Sendable, Hashable {
    /// No configured signing provider answers for `pubkey` (64-char hex) --
    /// the exact identity FROZEN at acceptance, never whichever account is
    /// current now. Re-armed only when THIS account's provider becomes
    /// available, and re-emitted verbatim on restart replay.
    ///
    /// **No clock ever ends this.** A device whose signer is simply not
    /// plugged in yet is not a device whose write failed; the app's own
    /// decision is the only other exit, and it is two calls: cancel the
    /// write, then remove the terminal queue entry it leaves behind.
    ///
    /// This is the state a person has to be told about, and `inFlight` is the
    /// one it must never be confused with.
    case awaitingSigner(pubkey: String)
    /// A signer for `pubkey` (64-char hex) HAS the request and has not
    /// answered yet -- the ordinary state of every healthy write between
    /// acceptance and signature promotion.
    ///
    /// Transient and normal: it ends when the signer answers (`signed` or
    /// `refused`), or falls back to `awaitingSigner` if that signer becomes
    /// unavailable. Nothing here is a reason to trouble a user.
    case inFlight(pubkey: String)
    case signed(eventId: String)
    /// The signer answered and said no. Terminal for the whole write.
    case refused(reason: String)

    init(_ ffi: FfiSigningState) {
        switch ffi {
        case .awaitingSigner(let pubkey): self = .awaitingSigner(pubkey: pubkey)
        case .inFlight(let pubkey): self = .inFlight(pubkey: pubkey)
        case .signed(let eventId): self = .signed(eventId: eventId)
        case .refused(let reason): self = .refused(reason: reason)
        }
    }
}

/// Why a relay lane is not attempting right now. Every case is a fact about
/// the lane; none of them is a deadline.
public enum RelayWaiting: Sendable, Hashable {
    /// Offline time consumes no attempt ordinal, so being offline can never
    /// spend the give-up ceiling.
    case notConnected
    case needsAuth
    /// The last attempt failed in a way that permits another one, and
    /// `cause`/`detail` say WHY -- "we will try again" and "we will try again
    /// because the relay rate-limited us" are different messages and only the
    /// second one can be acted on.
    case backingOff(
        attempt: UInt64,
        eligibleAt: UInt64,
        cause: RetryCause,
        detail: String?
    )
    /// The lane is owned and nonterminal, but a durable fact about it could
    /// not be committed -- the local disk is refusing writes. No wire EVENT
    /// was emitted. Also latched onto the queue entry and never cleared by a
    /// later ack.
    case persistenceStalled(detail: String)

    init(_ ffi: FfiRelayWaiting) {
        switch ffi {
        case .notConnected: self = .notConnected
        case .needsAuth: self = .needsAuth
        case .backingOff(let attempt, let eligibleAt, let cause, let detail):
            self = .backingOff(
                attempt: attempt,
                eligibleAt: eligibleAt,
                cause: RetryCause(cause),
                detail: detail
            )
        case .persistenceStalled(let detail): self = .persistenceStalled(detail: detail)
        }
    }
}

/// What is true at ONE relay. `.published`, `.rejected`, `.authFailed` and
/// `.gaveUp` are terminal for that relay; `.waiting` and `.sent` are not.
public enum RelayState: Sendable, Hashable {
    case waiting(RelayWaiting)
    /// Transport proved socket write + flush. Not an ack, and not terminal.
    case sent(attempt: UInt64, writtenAt: UInt64)
    case published
    /// The relay authenticated the identity and refused THIS EVENT. The
    /// repair is to the event.
    case rejected(reason: String)
    /// The write could not be authenticated HERE. Deliberately NOT folded
    /// into `.rejected`: `source` keeps an app's own decision not to
    /// authenticate from being shown to a user as a relay refusing them.
    case authFailed(pubkey: String, source: AuthDenialSource, reason: String)
    /// The attempt ceiling was reached at this relay. Terminal HERE and
    /// nowhere else: three relays published and one given up on is a success
    /// with a footnote, not a failed write.
    case gaveUp

    /// Whether this relay will produce another fact.
    public var isTerminal: Bool {
        switch self {
        case .published, .rejected, .authFailed, .gaveUp: return true
        case .waiting, .sent: return false
        }
    }

    init(_ ffi: FfiRelayState) {
        switch ffi {
        case .waiting(let waiting): self = .waiting(RelayWaiting(waiting))
        case .sent(let attempt, let writtenAt):
            self = .sent(attempt: attempt, writtenAt: writtenAt)
        case .published: self = .published
        case .rejected(let reason): self = .rejected(reason: reason)
        case .authFailed(let pubkey, let source, let reason):
            self = .authFailed(
                pubkey: pubkey,
                source: AuthDenialSource(source),
                reason: reason
            )
        case .gaveUp: self = .gaveUp
        }
    }
}

/// Why a write ended without going anywhere.
public enum NotSentReason: Sendable, Hashable {
    case cancelled
    /// The accepted signing request was refused or failed, and no EVENT bytes
    /// crossed the local transport handoff.
    case signerRefused
    /// A newer accepted write won the same replaceable coordinate, and NMP
    /// proved the older bytes did not cross the local transport handoff. Not a
    /// failure -- for an app renewing presence it is the steady state.
    case superseded

    init(_ ffi: FfiNotSentReason) {
        switch ffi {
        case .cancelled: self = .cancelled
        case .signerRefused: self = .signerRefused
        case .superseded: self = .superseded
        }
    }
}

/// Why the acceptance door said no.
public enum RefuseReason: Sendable, Hashable {
    case alreadyExpired
    case tombstoned
    case replaceableBaseOnRegularEvent
    /// A whole-value replacement lost its compare-and-swap.
    ///
    /// BOTH ids are kept, and that is what makes the failure recoverable
    /// without the user: fetch `actual`, reapply the change and resubmit
    /// silently. Reduced to a string you could only tell them to redo it.
    case replaceableBaseChanged(expected: String?, actual: String?)

    init(_ ffi: FfiRefuseReason) {
        switch ffi {
        case .alreadyExpired: self = .alreadyExpired
        case .tombstoned: self = .tombstoned
        case .replaceableBaseOnRegularEvent: self = .replaceableBaseOnRegularEvent
        case .replaceableBaseChanged(let expected, let actual):
            self = .replaceableBaseChanged(expected: expected, actual: actual)
        }
    }
}

/// The whole-write terminal. Exactly one of these ends every receipt stream,
/// so a stream can never end in silence and you can always tell a finished
/// write from a dropped subscription.
public enum WriteOutcome: Sendable, Hashable {
    /// The destination set is CLOSED and every relay in it is terminal. What
    /// happened at each is the per-relay facts; this says only that no more
    /// are coming.
    case settled
    /// Routing finished -- knowledge is exhausted -- and named zero relays.
    /// Terminal: there is nowhere to publish. Distinct from a route still
    /// resolving, which parks forever.
    case noDestination
    case notSent(NotSentReason)
    /// A newer replaceable write retired this obligation after its bytes may
    /// already have crossed the local transport handoff. It will not retry.
    case superseded
    /// The store answered the acceptance instruction with a semantic no. The
    /// write is in custody as a permanently-failed entry: one row, payload
    /// intact, readable and removable through bounded `Engine.publishQueue` pages.
    case refused(RefuseReason)

    init(_ ffi: FfiWriteOutcome) {
        switch ffi {
        case .settled: self = .settled
        case .noDestination: self = .noDestination
        case .notSent(let reason): self = .notSent(NotSentReason(reason))
        case .superseded: self = .superseded
        case .refused(let reason): self = .refused(RefuseReason(reason))
        }
    }
}

/// One fact about a write, delivered on its receipt stream.
///
/// Acceptance is deliberately ABSENT: `publish` returning a receipt IS
/// acceptance, so you never ask the stream whether your write was taken.
/// Settlement is INSPECTED, never AWAITED -- a locally accepted write is
/// already visible through your own live query, reporting cache and zero
/// relays. Never block a UI on this.
public enum WriteFact: Sendable, Hashable {
    case signing(SigningState)
    case relay(relay: String, state: RelayState)
    /// The relays this write is INTENDED for, and whether resolution can
    /// still change its mind. `complete` flips on settled RESOLUTION, never
    /// on delivery, so `complete == true` with nothing published yet is
    /// "sending 0 of n". This is the settlement denominator.
    ///
    /// `complete == false` with an empty set is a write still learning where
    /// it goes; it parks indefinitely and NOTHING expires it. `complete ==
    /// true` with an empty set is `.outcome(.noDestination)`.
    ///
    /// `awaitingAuthorRoutes` is WHY resolution is still open, as 64-char hex
    /// public keys rather than as a sentence: every author whose routes this
    /// write is still waiting on, in sorted key order. A later positive route
    /// fact for any one of them is the only thing that can move the picture,
    /// so the set is both the reason to show and the list of repairs.
    /// Non-empty implies `complete == false`; a settled resolution names
    /// nobody. The converse does NOT hold: an open picture naming nobody is a
    /// write whose routing has not run at all because it is not signed yet,
    /// and `.signing` is the fact that says what it IS held on. Never a
    /// rendered message -- a park you can only print is a park you cannot act
    /// on.
    case destinations(relays: [String], complete: Bool, awaitingAuthorRoutes: [String])
    case outcome(WriteOutcome)

    init(_ ffi: FfiWriteFact) {
        switch ffi {
        case .signing(let state): self = .signing(SigningState(state))
        case .relay(let relay, let state):
            self = .relay(relay: relay, state: RelayState(state))
        case .destinations(let relays, let complete, let awaitingAuthorRoutes):
            self = .destinations(
                relays: relays,
                complete: complete,
                awaitingAuthorRoutes: awaitingAuthorRoutes
            )
        case .outcome(let outcome): self = .outcome(WriteOutcome(outcome))
        }
    }
}

/// One write in your publish queue, as you read it back.
///
/// Enumerating the queue answers "what have I got outstanding, and what went
/// wrong with it" without having held a receipt stream open since
/// acceptance. It is INSPECTION: nothing here blocks.
public struct PublishQueueEntry: Sendable, Hashable {
    public let receiptID: UInt64
    /// The frozen event id (64-char hex) -- the write's identity from
    /// acceptance onward, unchanged by signing.
    public let eventID: String
    /// The identity frozen at acceptance (64-char hex). Never re-resolved.
    public let pubkey: String
    public let acceptedAt: UInt64
    public let signing: SigningState
    public let relays: [String]
    public let routeComplete: Bool
    public let relayStates: [(relay: String, state: RelayState)]
    /// `nil` while the write is still in progress.
    public let outcome: WriteOutcome?
    /// LATCHED. Set the first time local persistence refused a durable fact
    /// for this write, and never cleared by a later success -- an operator
    /// must not lose the only signal that the disk is failing because a relay
    /// acked afterwards.
    public let persistenceFault: String?

    /// Whether this write will produce another fact.
    public var isTerminal: Bool { outcome != nil }

    init(_ ffi: FfiPublishQueueEntry) {
        receiptID = ffi.receiptId
        eventID = ffi.eventId
        pubkey = ffi.pubkey
        acceptedAt = ffi.acceptedAt
        signing = SigningState(ffi.signing)
        relays = ffi.relays
        routeComplete = ffi.routeComplete
        relayStates = ffi.relayStates.map { (relay: $0.relay, state: RelayState($0.state)) }
        outcome = ffi.outcome.map(WriteOutcome.init)
        persistenceFault = ffi.persistenceFault
    }

    public static func == (lhs: PublishQueueEntry, rhs: PublishQueueEntry) -> Bool {
        lhs.receiptID == rhs.receiptID
            && lhs.eventID == rhs.eventID
            && lhs.pubkey == rhs.pubkey
            && lhs.acceptedAt == rhs.acceptedAt
            && lhs.signing == rhs.signing
            && lhs.relays == rhs.relays
            && lhs.routeComplete == rhs.routeComplete
            && lhs.relayStates.map(\.relay) == rhs.relayStates.map(\.relay)
            && lhs.relayStates.map(\.state) == rhs.relayStates.map(\.state)
            && lhs.outcome == rhs.outcome
            && lhs.persistenceFault == rhs.persistenceFault
    }

    public func hash(into hasher: inout Hasher) {
        hasher.combine(receiptID)
        hasher.combine(eventID)
        hasher.combine(outcome)
    }
}
