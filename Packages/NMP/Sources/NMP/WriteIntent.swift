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
public struct WriteIntent: Sendable, Hashable {
    public var payload: WritePayload
    public var durability: Durability
    public var routing: WriteRouting

    public init(
        payload: WritePayload,
        durability: Durability,
        routing: WriteRouting
    ) {
        self.payload = payload
        self.durability = durability
        self.routing = routing
    }

    func toFfi() -> FfiWriteIntent {
        FfiWriteIntent(
            payload: payload.toFfi(),
            durability: durability.toFfi(),
            routing: routing.toFfi()
        )
    }
}

/// Every state a publish's receipt stream may report (ledger #9: enqueue is
/// not converged -- many of these may arrive per publish, one per relay for
/// the terminal states).
public enum WriteStatus: Sendable, Hashable {
    case accepted
    case awaitingCapability
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
        case .awaitingCapability: self = .awaitingCapability
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
