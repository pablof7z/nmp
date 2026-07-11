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

/// A caller's publish request. `pubkey` must be the account being published
/// as (see `NMPEngine.setActiveAccount`); the payload is always an unsigned
/// template -- the key lives engine-side and signs it there.
public struct WriteIntent: Sendable, Hashable {
    public var pubkey: String
    public var createdAt: UInt64
    public var kind: UInt16
    public var tags: [[String]]
    public var content: String
    public var durability: Durability
    public var routing: WriteRouting

    public init(
        pubkey: String,
        createdAt: UInt64,
        kind: UInt16,
        tags: [[String]] = [],
        content: String,
        durability: Durability,
        routing: WriteRouting
    ) {
        self.pubkey = pubkey
        self.createdAt = createdAt
        self.kind = kind
        self.tags = tags
        self.content = content
        self.durability = durability
        self.routing = routing
    }

    func toFfi() -> FfiWriteIntent {
        FfiWriteIntent(
            pubkey: pubkey,
            createdAt: createdAt,
            kind: kind,
            tags: tags,
            content: content,
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
