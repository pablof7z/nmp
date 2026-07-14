import NMPFFI

/// Event fields to sign with NMP's active account. The author is deliberately
/// absent so callers cannot redirect signing to another registered identity.
public struct NMPUnsignedEvent: Sendable, Hashable {
    public var createdAt: UInt64
    public var kind: UInt16
    public var tags: [[String]]
    public var content: String

    public init(createdAt: UInt64, kind: UInt16, tags: [[String]], content: String) {
        self.createdAt = createdAt
        self.kind = kind
        self.tags = tags
        self.content = content
    }

    func toFfi() -> FfiSignEventRequest {
        FfiSignEventRequest(createdAt: createdAt, kind: kind, tags: tags, content: content)
    }
}

public struct NMPSignedEvent: Sendable, Hashable {
    public let id: String
    public let pubkey: String
    public let createdAt: UInt64
    public let kind: UInt16
    public let tags: [[String]]
    public let content: String
    public let sig: String

    init(_ event: FfiSignedEvent) {
        id = event.id
        pubkey = event.pubkey
        createdAt = event.createdAt
        kind = event.kind
        tags = event.tags
        content = event.content
        sig = event.sig
    }
}

extension NMPEngine {
    /// Sign one exact event without publishing, storing, or routing it.
    public func signEvent(_ event: NMPUnsignedEvent) async throws -> NMPSignedEvent {
        try await Task.detached { [ffi] in
            let signed = try nmpRethrowing { try ffi.signEvent(request: event.toFfi()) }
            return NMPSignedEvent(signed)
        }.value
    }
}
