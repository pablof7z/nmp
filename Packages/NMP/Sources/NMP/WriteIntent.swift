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

/// Where a write is routed. `.privateNarrow`'s `relays` is the fixed,
/// fail-closed set itself -- an empty array is exactly how "unroutable" is
/// expressed; there is no widen operation on this wire.
public enum WriteRouting: Sendable, Hashable {
    case authorOutbox
    case toInboxes([String])
    case privateNarrow([String])

    func toFfi() -> FfiWriteRouting {
        switch self {
        case .authorOutbox: return .authorOutbox
        case .toInboxes(let recipients): return .toInboxes(recipients: recipients)
        case .privateNarrow(let relays): return .privateNarrow(relays: relays)
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
    case sent(relay: String)
    case acked(relay: String)
    case rejected(relay: String, reason: String)
    case gaveUp(relay: String)
    case failed(reason: String)

    init(_ ffi: FfiWriteStatus) {
        switch ffi {
        case .accepted: self = .accepted
        case .awaitingCapability: self = .awaitingCapability
        case .signed(let eventId): self = .signed(eventId: eventId)
        case .routed(let relays): self = .routed(relays: relays)
        case .sent(let relay): self = .sent(relay: relay)
        case .acked(let relay): self = .acked(relay: relay)
        case .rejected(let relay, let reason): self = .rejected(relay: relay, reason: reason)
        case .gaveUp(let relay): self = .gaveUp(relay: relay)
        case .failed(let reason): self = .failed(reason: reason)
        }
    }
}
