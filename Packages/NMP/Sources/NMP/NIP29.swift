// The NIP-29 door, in ergonomic Swift shape (#1033): a relay scope you name
// once, narrowed to a group when you want a specific one.
//
//   let scope = try NMPRelayScope.on(["wss://relay-a.example.com"])
//   let mine = try NMPGroupPredicate.memberListIncludes(.reactive(.activePubkey))
//   let query = try scope.groupsWhere(mine)               // one branch per host
//
//   let group = scope.group("photographers")               // narrows, contacts nothing
//   let query = try group.read(NMPFilter(kinds: [9]))       // #h-scoped, per-host branches
//   let status = try group.publish(engine: engine, author: pubkeyHex, kind: 9, content: "hi")
//   for try await frame in status { ... }
//
// `NMPRelayScope`/`NMPGroup`/`NMPGroupPredicate` wrap the opaque
// `FfiRelayScope`/`FfiGroup`/`FfiGroupPredicate` UniFFI objects exactly like
// `BlossomAuthorization` wraps `FfiBlossomAuthorization` in Blossom.swift --
// a proven Rust value carried across the boundary, never a second mirrored
// copy of NIP-29's own vocabulary. Neither type exposes its retained hosts
// or group id back out.
//
// Deliberately absent, same as before #1033: a fixed group-content kind
// catalog and a kind:9 composer -- NIP-29 owns neither; C7 and client
// notification policy remain independently optional (#838). Also absent:
// any second projection of a NIP-51 Simple-groups entry -- `NIP51.swift`
// keeps that one shape.
//
// Deleted in this change, no alias: `groupDiscoveryDemand(host:)` and
// `Group`'s single-host constructor. A group can live on more than one
// relay; the single-host door is gone, not deprecated.

import NMPFFI

/// The relays a NIP-29 group lives on -- named once, retained privately, and
/// never asked for again (`nmp::nip29::RelayScope`/`FfiRelayScope` mirror).
public final class NMPRelayScope: @unchecked Sendable {
    let ffi: FfiRelayScope

    private init(_ ffi: FfiRelayScope) {
        self.ffi = ffi
    }

    /// Name the relays a NIP-29 group lives on. Each host is parsed with the
    /// same rule every other relay-URL input in this package uses
    /// (`NMPError.invalidRelayUrl`); an empty set throws
    /// `NMPError.emptyRelayScope` -- a group must be hosted somewhere.
    public static func on(_ hosts: [String]) throws -> NMPRelayScope {
        try NMPRelayScope(nmpRethrowing { try FfiRelayScope.on(hosts: hosts) })
    }

    /// Narrow to one group id, keeping the same hosts. Contacts nothing.
    public func group(_ groupID: String) -> NMPGroup {
        NMPGroup(ffi.group(groupId: groupID))
    }

    /// Groups on these relays matching a composable discovery predicate. One
    /// complete branch per host, folded into ONE `NMPLiveQuery` --
    /// `NMPEngine.observe(_:)` takes it directly, never a per-host demand
    /// list the app has to merge itself.
    public func groupsWhere(_ predicate: NMPGroupPredicate) throws -> NMPLiveQuery {
        try NMPLiveQuery(nmpRethrowing { try ffi.groupsWhere(predicate: predicate.ffi) })
    }
}

extension NMPEngine {
    /// The concrete FFI engine handle `FfiGroup`'s write operations require
    /// (`Arc<NmpEngine>` in Rust, the concrete `NmpEngine` class in Swift --
    /// not `NmpEngineProtocol`, the testability existential every other
    /// door in this package accepts through `NMPEngine.ffi`). Every
    /// app-constructed `NMPEngine` wraps a real `NmpEngine`; only this
    /// package's own internal test-fake seam (`NMPEngine.init(ffi:)`) could
    /// produce anything else, and a NIP-29 group write has no meaning
    /// against a fake FFI object -- there is no real `Arc<NmpEngine>`
    /// pointer to hand across the boundary for one.
    var concreteFfiEngine: NmpEngine {
        guard let concrete = ffi as? NmpEngine else {
            preconditionFailure(
                "NMPGroup write operations require a real NmpEngine; a test-only fake was supplied"
            )
        }
        return concrete
    }
}

/// One NIP-29 group, on the relays its scope named (`nmp::nip29::Group`/
/// `FfiGroup` mirror). An identity, not a subscription: obtaining one (via
/// `NMPRelayScope.group(_:)`) contacts nothing. The same value serves every
/// read and every write for a room's whole lifetime.
public final class NMPGroup: @unchecked Sendable {
    let ffi: FfiGroup

    init(_ ffi: FfiGroup) {
        self.ffi = ffi
    }

    /// Mint the read declaration for an app-supplied selection. A selection
    /// that already constrains `#h` throws
    /// `NMPError.groupCallerSuppliedContextConstraint` -- the retained group
    /// id is the sole semantic source of that row. Hand the result to
    /// `NMPEngine.observe(_:)`.
    public func read(_ selection: NMPFilter) throws -> NMPLiveQuery {
        try NMPLiveQuery(nmpRethrowing { try ffi.read(selection: selection.toFfi()) })
    }

    /// Ask whether an already-signed event belongs to this group, without
    /// building a write out of it.
    public func validateContext(_ event: NMPSignedEvent) throws {
        try nmpRethrowing { try ffi.validateContext(event: event.toFfiSignedEvent()) }
    }

    /// Publish an unsigned draft into the group, as `authorPubkeyHex`
    /// (exact decoded hex, never the active-account selector -- a semantic
    /// group write freezes who is writing at composition time, #878).
    public func publish(
        engine: NMPEngine,
        authorPubkeyHex: String,
        kind: UInt16,
        tags: [[String]] = [],
        content: String = "",
        createdAt: UInt64? = nil
    ) throws -> NMPGroupWriteStatus {
        let receipts = try nmpRethrowing {
            try ffi.publish(
                engine: engine.concreteFfiEngine,
                author: authorPubkeyHex,
                builder: FfiEventBuilder(kind: kind, tags: tags, content: content, createdAt: createdAt)
            )
        }
        return NMPGroupWriteStatus(handle: receipts)
    }

    /// Publish an ALREADY-SIGNED event into the group. The `h` it already
    /// carries is validated, never appended or repaired -- see
    /// `validateContext(_:)`'s doc for the exact refusals.
    public func publishSigned(
        engine: NMPEngine,
        event: NMPSignedEvent
    ) throws -> NMPGroupWriteStatus {
        let receipts = try nmpRethrowing {
            try ffi.publishSigned(engine: engine.concreteFfiEngine, event: event.toFfiSignedEvent())
        }
        return NMPGroupWriteStatus(handle: receipts)
    }

    /// kind:9021 -- ask to join. Publishable with no subscription at all.
    public func joinRequest(
        engine: NMPEngine,
        authorPubkeyHex: String,
        inviteCode: String? = nil
    ) throws -> NMPGroupWriteStatus {
        let receipts = try nmpRethrowing {
            try ffi.joinRequest(engine: engine.concreteFfiEngine, author: authorPubkeyHex, inviteCode: inviteCode)
        }
        return NMPGroupWriteStatus(handle: receipts)
    }

    /// kind:9022 -- leave.
    public func leaveRequest(
        engine: NMPEngine,
        authorPubkeyHex: String
    ) throws -> NMPGroupWriteStatus {
        let receipts = try nmpRethrowing {
            try ffi.leaveRequest(engine: engine.concreteFfiEngine, author: authorPubkeyHex)
        }
        return NMPGroupWriteStatus(handle: receipts)
    }

    /// kind:9000 -- add a member, optionally with a role.
    public func addUser(
        engine: NMPEngine,
        authorPubkeyHex: String,
        pubkeyHex: String,
        role: String? = nil
    ) throws -> NMPGroupWriteStatus {
        let receipts = try nmpRethrowing {
            try ffi.addUser(
                engine: engine.concreteFfiEngine, author: authorPubkeyHex, pubkey: pubkeyHex, role: role
            )
        }
        return NMPGroupWriteStatus(handle: receipts)
    }

    /// kind:9001 -- remove a member.
    public func removeUser(
        engine: NMPEngine,
        authorPubkeyHex: String,
        pubkeyHex: String
    ) throws -> NMPGroupWriteStatus {
        let receipts = try nmpRethrowing {
            try ffi.removeUser(engine: engine.concreteFfiEngine, author: authorPubkeyHex, pubkey: pubkeyHex)
        }
        return NMPGroupWriteStatus(handle: receipts)
    }

    /// kind:9002 -- set the group's display fields. An omitted field emits
    /// no tag at all, so it is left untouched rather than cleared.
    public func editMetadata(
        engine: NMPEngine,
        authorPubkeyHex: String,
        name: String? = nil,
        about: String? = nil
    ) throws -> NMPGroupWriteStatus {
        let receipts = try nmpRethrowing {
            try ffi.editMetadata(
                engine: engine.concreteFfiEngine, author: authorPubkeyHex, name: name, about: about
            )
        }
        return NMPGroupWriteStatus(handle: receipts)
    }

    /// kind:9005 -- delete one group-hosted event.
    public func deleteEvent(
        engine: NMPEngine,
        authorPubkeyHex: String,
        eventID: String
    ) throws -> NMPGroupWriteStatus {
        let receipts = try nmpRethrowing {
            try ffi.deleteEvent(engine: engine.concreteFfiEngine, author: authorPubkeyHex, eventId: eventID)
        }
        return NMPGroupWriteStatus(handle: receipts)
    }

    /// kind:9007 -- create the group at its hosts.
    public func createGroup(
        engine: NMPEngine,
        authorPubkeyHex: String
    ) throws -> NMPGroupWriteStatus {
        let receipts = try nmpRethrowing {
            try ffi.createGroup(engine: engine.concreteFfiEngine, author: authorPubkeyHex)
        }
        return NMPGroupWriteStatus(handle: receipts)
    }

    /// kind:9008 -- delete the group from its hosts.
    public func deleteGroup(
        engine: NMPEngine,
        authorPubkeyHex: String
    ) throws -> NMPGroupWriteStatus {
        let receipts = try nmpRethrowing {
            try ffi.deleteGroup(engine: engine.concreteFfiEngine, author: authorPubkeyHex)
        }
        return NMPGroupWriteStatus(handle: receipts)
    }

    /// kind:9009 -- mint an invite code redeemable by `joinRequest`.
    public func createInvite(
        engine: NMPEngine,
        authorPubkeyHex: String,
        code: String
    ) throws -> NMPGroupWriteStatus {
        let receipts = try nmpRethrowing {
            try ffi.createInvite(engine: engine.concreteFfiEngine, author: authorPubkeyHex, code: code)
        }
        return NMPGroupWriteStatus(handle: receipts)
    }
}

/// A composable NIP-29 discovery predicate (`nmp::nip29::GroupPredicate`/
/// `FfiGroupPredicate` mirror). Opaque by design -- built with
/// `.memberListIncludes`/`.adminListIncludes` and composed with
/// `union`/`intersect`/`minus`, then handed to
/// `NMPRelayScope.groupsWhere(_:)`.
public final class NMPGroupPredicate: @unchecked Sendable {
    let ffi: FfiGroupPredicate

    private init(_ ffi: FfiGroupPredicate) {
        self.ffi = ffi
    }

    /// Groups whose observed kind:39002 member-list evidence names
    /// `subjects`. Inclusion is evidence, never exact state -- absence is
    /// not evidence of non-membership.
    public static func memberListIncludes(_ subjects: NMPBinding) throws -> NMPGroupPredicate {
        try NMPGroupPredicate(
            nmpRethrowing { try NMPFFI.memberListIncludes(subjects: subjects.toFfi()) }
        )
    }

    /// Groups whose observed kind:39001 admin-list evidence names
    /// `subjects`. Evidence-scoped exactly like `memberListIncludes`.
    public static func adminListIncludes(_ subjects: NMPBinding) throws -> NMPGroupPredicate {
        try NMPGroupPredicate(
            nmpRethrowing { try NMPFFI.adminListIncludes(subjects: subjects.toFfi()) }
        )
    }

    /// Groups matching this predicate OR any of `others`.
    public func union(_ others: [NMPGroupPredicate]) -> NMPGroupPredicate {
        NMPGroupPredicate(ffi.union(others: others.map { $0.ffi }))
    }

    /// Groups matching this predicate AND all of `others`.
    public func intersect(_ others: [NMPGroupPredicate]) -> NMPGroupPredicate {
        NMPGroupPredicate(ffi.intersect(others: others.map { $0.ffi }))
    }

    /// Groups matching this predicate and none of `others`.
    public func minus(_ others: [NMPGroupPredicate]) -> NMPGroupPredicate {
        NMPGroupPredicate(ffi.minus(others: others.map { $0.ffi }))
    }
}

extension NMPSignedEvent {
    func toFfiSignedEvent() -> FfiSignedEvent {
        FfiSignedEvent(
            id: id, pubkey: pubkey, createdAt: createdAt, kind: kind, tags: tags,
            content: content, sig: signature
        )
    }
}

extension NmpGroupReceiptStream: NMPPullHandle {
    func pullNext() async throws -> FfiWriteStatus? { try await next() }
}

/// The ordered `WriteStatus` facts one group write's write reaches, pulled
/// from its untracked receipt handle (#1033). UNLIKE `ReceiptStatus` this
/// carries NO receipt id: every `NMPGroup` write reaches the engine's
/// untracked publish door (never `publish_tracked`), because the
/// store-issued receipt-id namespace is a `publish`-door concern the group
/// scope has no reason to surface. Iterate with `for try await`; the handle
/// is single-consumer, so a second concurrent iterator surfaces
/// `NMPError.concurrentNext` rather than hanging.
public struct NMPGroupWriteStatus: AsyncSequence, Sendable {
    public typealias Element = WriteStatus

    private let handle: NmpGroupReceiptStream
    private let iteratorGate = NMPPullIteratorGate()

    init(handle: NmpGroupReceiptStream) {
        self.handle = handle
    }

    public func makeAsyncIterator() -> Iterator {
        let core = NMPPullIteratorCore(handle: handle, iteratorGate: iteratorGate) { status in
            WriteStatus(status)
        }
        return Iterator(core: core)
    }

    public struct Iterator: AsyncIteratorProtocol {
        let core: NMPPullIteratorCore<NmpGroupReceiptStream, WriteStatus>

        public mutating func next() async throws -> WriteStatus? {
            try await core.next()
        }
    }

    /// Stop delivering live status frames to this stream. Idempotent.
    public func cancel() {
        handle.cancel()
    }
}
