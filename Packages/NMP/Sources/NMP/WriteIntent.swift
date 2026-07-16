// The write noun, in ergonomic Swift shape (M4 plan §9).

import NMPFFI

/// A durability PROPERTY of a write (not a routing choice).
public enum Durability: Sendable, Hashable {
    case durable
    case ephemeral
    case atMostOnce

    func toFfi() -> FfiDurability {
        switch self {
        case .durable: return .durable
        case .ephemeral: return .ephemeral
        case .atMostOnce: return .atMostOnce
        }
    }
}

/// Where a write is routed. There is deliberately no `.privateNarrow` case
/// (#22/#52): a private/narrow route must come from a trusted protocol
/// module's own resolved logic, never a raw relay-URL string an app hands
/// across this boundary with no way to prove it is actually private --
/// exactly the "route escape hatch" #22's canonical design rules out. See
/// `FfiWriteRouting`'s doc.
public enum WriteRouting: Sendable, Hashable {
    case authorOutbox
    case toInboxes([String])

    func toFfi() -> FfiWriteRouting {
        switch self {
        case .authorOutbox: return .authorOutbox
        case .toInboxes(let recipients): return .toInboxes(recipients: recipients)
        }
    }
}

/// The event payload of a write intent (`FfiWritePayload` mirror). VISION
/// P: signing and publishing are ORTHOGONAL stages -- `.unsigned` is a
/// template whose `pubkey` names the account being published as (see
/// `NMPEngine.setActiveAccount`); the key lives engine-side and signs it
/// there. `.signed` (#32, the M5 unlock) is a caller that already holds a
/// validly-signed event -- an external signer / NIP-46 bunker, or a
/// verbatim republish -- and hands its fields across as-is: the engine
/// verifies then publishes it exactly as given, never re-signing, mutating
/// a tag, or recomputing an id.
public enum WritePayload: Sendable, Hashable {
    case unsigned(pubkey: String, createdAt: UInt64, kind: UInt16, tags: [[String]], content: String)
    case signed(
        id: String, pubkey: String, createdAt: UInt64, kind: UInt16, tags: [[String]],
        content: String, sig: String)

    func toFfi() -> FfiWritePayload {
        switch self {
        case .unsigned(let pubkey, let createdAt, let kind, let tags, let content):
            return .unsigned(pubkey: pubkey, createdAt: createdAt, kind: kind, tags: tags, content: content)
        case .signed(let id, let pubkey, let createdAt, let kind, let tags, let content, let sig):
            return .signed(
                id: id, pubkey: pubkey, createdAt: createdAt, kind: kind, tags: tags, content: content,
                sig: sig)
        }
    }
}

/// A caller's publish request (`FfiWriteIntent` mirror).
///
/// `identityOverride` (#47) is the identity this ONE write is published
/// under, as 64-char hex or bech32 `npub`. `nil` -- the default every
/// existing call site keeps -- means the active account at acceptance time
/// (see `NMPEngine.setActiveAccount`), unchanged. Non-`nil` must name
/// exactly the payload's own author; misuse fails closed, never silently
/// retargets: a malformed string throws synchronously from `publish`
/// (`NMPError.invalidPublicKey`), while a well-formed-but-mismatched
/// override surfaces as `WriteStatus.failed` on the receipt stream with no
/// `.accepted` before it. An override naming a pubkey with no registered
/// signer parks as `.awaitingCapability` until that capability attaches;
/// acceptance pins the override, so a later `setActiveAccount` cannot
/// retarget the write. `WriteStatus.awaitingCapability`'s associated
/// `pubkey` (#47 Unit B) is the exact frozen identity parked -- the
/// override when one was given, else the active account at publish time --
/// never a different, later-active account.
public struct WriteIntent: Sendable, Hashable {
    public var payload: WritePayload
    public var durability: Durability
    public var routing: WriteRouting
    public var identityOverride: String?
    /// Crash-safe client correlation token (#591). `nil` -- the default --
    /// opts this write out of correlation entirely. A non-`nil` token is
    /// validated by `nmp_grammar::CorrelationToken::new` on the way across
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
        durability: Durability,
        routing: WriteRouting,
        identityOverride: String? = nil,
        correlation: String? = nil
    ) {
        self.payload = payload
        self.durability = durability
        self.routing = routing
        self.identityOverride = identityOverride
        self.correlation = correlation
    }

    func toFfi() -> FfiWriteIntent {
        FfiWriteIntent(
            payload: payload.toFfi(),
            durability: durability.toFfi(),
            routing: routing.toFfi(),
            identityOverride: identityOverride,
            correlation: correlation
        )
    }
}

/// Every state a publish's receipt stream may report (ledger #9: enqueue is
/// not converged -- many of these may arrive per publish, one per relay for
/// the terminal states).
public enum WriteStatus: Sendable, Hashable {
    case accepted
    case cancelled
    /// #47 Unit B: `pubkey` is the exact frozen identity (64-char hex) no
    /// registered signer currently answers for. Retained, not terminal --
    /// re-arrives verbatim on restart replay and resumes only when a
    /// signer for THIS pubkey attaches, never a different one.
    case awaitingCapability(pubkey: String)
    case signed(eventId: String)
    case routed(relays: [String])
    case awaitingRelay(relay: String)
    case awaitingAuth(relay: String)
    case retryEligible(relay: String, attempt: UInt64, eligibleAt: UInt64)
    case handoffAmbiguous(relay: String, attempt: UInt64, observedAt: UInt64)
    case sent(relay: String, attempt: UInt64, writtenAt: UInt64)
    case acked(relay: String)
    case rejected(relay: String, reason: String)
    case gaveUp(relay: String)
    case persistenceBlocked(relay: String)
    case routePersistenceBlocked(relay: String)
    case outcomeUnknown(relay: String)
    case replaceableConflict(expected: String?, actual: String?)
    case failed(reason: String)

    init(_ ffi: FfiWriteStatus) {
        switch ffi {
        case .accepted: self = .accepted
        case .cancelled: self = .cancelled
        case .awaitingCapability(let pubkey): self = .awaitingCapability(pubkey: pubkey)
        case .signed(let eventId): self = .signed(eventId: eventId)
        case .routed(let relays): self = .routed(relays: relays)
        case .awaitingRelay(let relay): self = .awaitingRelay(relay: relay)
        case .awaitingAuth(let relay): self = .awaitingAuth(relay: relay)
        case .retryEligible(let relay, let attempt, let eligibleAt):
            self = .retryEligible(relay: relay, attempt: attempt, eligibleAt: eligibleAt)
        case .handoffAmbiguous(let relay, let attempt, let observedAt):
            self = .handoffAmbiguous(relay: relay, attempt: attempt, observedAt: observedAt)
        case .sent(let relay, let attempt, let writtenAt):
            self = .sent(relay: relay, attempt: attempt, writtenAt: writtenAt)
        case .acked(let relay): self = .acked(relay: relay)
        case .rejected(let relay, let reason): self = .rejected(relay: relay, reason: reason)
        case .gaveUp(let relay): self = .gaveUp(relay: relay)
        case .persistenceBlocked(let relay): self = .persistenceBlocked(relay: relay)
        case .routePersistenceBlocked(let relay): self = .routePersistenceBlocked(relay: relay)
        case .outcomeUnknown(let relay): self = .outcomeUnknown(relay: relay)
        case .replaceableConflict(let expected, let actual):
            self = .replaceableConflict(expected: expected, actual: actual)
        case .failed(let reason): self = .failed(reason: reason)
        }
    }
}
