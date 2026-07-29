// NIP-29's native Group gate (#1015). One opaque identity mints ordinary
// demand and compose-and-publishes through NMP's canonical tracked write
// lifecycle. It never becomes a subscription, event-schema catalog, signer,
// route, retry loop, or receipt type of its own.

import NMPFFI

/// Group discovery (kind:39000) pinned to `host` (#108). Throws
/// `NMPError.invalidRelayUrl` if `host` doesn't parse.
public func groupDiscoveryDemand(host: String) throws -> NMPDemand {
    try NMPDemand(nmpRethrowing { try NMPFFI.groupDiscoveryDemand(host: host) })
}

/// One NIP-29 group identity retained as an opaque host/group pair.
///
/// Construction performs no I/O and requires neither a subscription nor an
/// active account. Use `demand(_:)` with `NMPEngine.observe(_:)` for reads;
/// every write method returns the ordinary `Receipt`.
public final class NMPGroup: Sendable {
    private let ffi: NmpGroupProtocol

    public init(host: String, id: String) throws {
        ffi = try nmpRethrowing {
            try NmpGroup(host: host, groupId: id)
        }
    }

    /// Apply this group's retained host and context to an app-selected filter.
    /// Observation remains exclusively `NMPEngine.observe(_:)`.
    public func demand(_ selection: NMPFilter) throws -> NMPDemand {
        try NMPDemand(nmpRethrowing {
            try ffi.demand(selection: selection.toFfi())
        })
    }

    /// Contextualize a complete unsigned builder and publish it through the
    /// canonical tracked lifecycle. The retained group identity is the only
    /// source of group context and host routing.
    public func publish(
        _ builder: NMPEventBuilder,
        using engine: NMPEngine,
        correlation: String? = nil
    ) async throws -> Receipt {
        let handle = try nmpRethrowing {
            try ffi.publish(
                engine: engine.ffi,
                builder: builder.toFfi(),
                correlation: correlation
            )
        }
        return Receipt(handle: handle)
    }

    /// Validate a pre-signed event's existing group context, then publish it
    /// without changing its bytes, signature, tags, or event id.
    public func publishSigned(
        _ event: NMPSignedEvent,
        using engine: NMPEngine,
        correlation: String? = nil
    ) async throws -> Receipt {
        let handle = try nmpRethrowing {
            try ffi.publishSigned(
                engine: engine.ffi,
                event: event.toFfi(),
                correlation: correlation
            )
        }
        return Receipt(handle: handle)
    }

    /// Request admission to the group (kind:9021). No read subscription is
    /// required.
    public func joinRequest(
        using engine: NMPEngine,
        inviteCode: String? = nil,
        correlation: String? = nil
    ) async throws -> Receipt {
        try receipt {
            try ffi.joinRequest(
                engine: engine.ffi,
                inviteCode: inviteCode,
                correlation: correlation
            )
        }
    }

    /// Leave the group (kind:9022).
    public func leaveRequest(
        using engine: NMPEngine,
        correlation: String? = nil
    ) async throws -> Receipt {
        try receipt {
            try ffi.leaveRequest(engine: engine.ffi, correlation: correlation)
        }
    }

    /// Add a member, optionally with a role (kind:9000).
    public func addUser(
        _ pubkey: String,
        role: String? = nil,
        using engine: NMPEngine,
        correlation: String? = nil
    ) async throws -> Receipt {
        try receipt {
            try ffi.addUser(
                engine: engine.ffi,
                pubkey: pubkey,
                role: role,
                correlation: correlation
            )
        }
    }

    /// Remove a member (kind:9001).
    public func removeUser(
        _ pubkey: String,
        using engine: NMPEngine,
        correlation: String? = nil
    ) async throws -> Receipt {
        try receipt {
            try ffi.removeUser(
                engine: engine.ffi,
                pubkey: pubkey,
                correlation: correlation
            )
        }
    }

    /// Update the supplied group metadata fields (kind:9002).
    public func editMetadata(
        name: String? = nil,
        about: String? = nil,
        using engine: NMPEngine,
        correlation: String? = nil
    ) async throws -> Receipt {
        try receipt {
            try ffi.editMetadata(
                engine: engine.ffi,
                name: name,
                about: about,
                correlation: correlation
            )
        }
    }

    /// Delete one group-hosted event (kind:9005).
    public func deleteEvent(
        _ eventID: String,
        using engine: NMPEngine,
        correlation: String? = nil
    ) async throws -> Receipt {
        try receipt {
            try ffi.deleteEvent(
                engine: engine.ffi,
                eventId: eventID,
                correlation: correlation
            )
        }
    }

    /// Create this group at its retained host (kind:9007).
    public func create(
        using engine: NMPEngine,
        correlation: String? = nil
    ) async throws -> Receipt {
        try receipt {
            try ffi.createGroup(engine: engine.ffi, correlation: correlation)
        }
    }

    /// Delete this group from its retained host (kind:9008).
    public func delete(
        using engine: NMPEngine,
        correlation: String? = nil
    ) async throws -> Receipt {
        try receipt {
            try ffi.deleteGroup(engine: engine.ffi, correlation: correlation)
        }
    }

    /// Create an invite code (kind:9009).
    public func createInvite(
        _ code: String,
        using engine: NMPEngine,
        correlation: String? = nil
    ) async throws -> Receipt {
        try receipt {
            try ffi.createInvite(
                engine: engine.ffi,
                code: code,
                correlation: correlation
            )
        }
    }

    private func receipt(
        _ operation: () throws -> NmpReceiptStream
    ) throws -> Receipt {
        Receipt(handle: try nmpRethrowing(operation))
    }
}
