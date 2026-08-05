// The NIP-29 door, in ergonomic Swift shape (#1033): a relay scope you name
// once, narrowed to a group when you want a specific one.
//
//   let scope = try NMPRelayScope.on(["wss://relay-a.example.com"])
//   let mine = try NMPGroupIds.memberListIncludes(.reactive(.activePubkey))
//   for try await snapshots in try scope.observeRecords(engine: engine,
//                                                       matching: .naming(mine),
//                                                       records: [.metadata]) { ... }
//
//   // A directory: every room this relay advertises, 250 per host.
//   for try await rooms in try scope.observeRecords(engine: engine,
//                                                   matching: .all,
//                                                   records: [.metadata],
//                                                   limit: 250) { ... }
//
//   // The room screen, five lines, no predicate and no id lookup:
//   let group = try NMPRelayScope.on([host]).group(roomID)
//   for try await room in try group.observeRecords(engine: engine,
//                                                  records: [.metadata, .admins, .members]) {
//       title = room.metadata?.name ?? roomID
//       members = room.members
//       iCanModerate = room.admins.contains { $0.pubkey == me }
//       isLoading = room.availability == .acquiring
//   }
//
//   let group = scope.group("photographers")               // narrows, contacts nothing
//   let query = try group.read(NMPFilter(kinds: [9]))       // #h-scoped, per-host branches
//   let receipt = try group.publish(engine: engine, authorPubkeyHex: pubkeyHex, kind: 9, content: "hi")
//   for try await frame in receipt.status { ... }
//
// `NMPRelayScope`/`NMPGroup`/`NMPGroupPredicate`/`NMPGroupIds` wrap the opaque
// `FfiRelayScope`/`FfiGroup`/`FfiGroupPredicate`/`FfiGroupIds` UniFFI objects exactly like
// `BlossomAuthorization` wraps `FfiBlossomAuthorization` in Blossom.swift --
// a proven Rust value carried across the boundary, never a second mirrored
// copy of NIP-29's own vocabulary. Neither type exposes its retained hosts
// or group id back out through an accessor, and no door hands back an
// unpublished intent that would yield them (#1292).
//
// Deliberately absent, same as before #1033: a fixed group-content kind
// catalog and a kind:9 composer -- NIP-29 owns neither; C7 and client
// notification policy remain independently optional (#838). Also absent:
// any second projection of a NIP-51 Simple-groups entry -- `NIP51.swift`
// keeps that one shape.
//
// Deleted in this change, no alias: the old single-host discovery-demand
// free function pinned to one relay, and `Group`'s single-host constructor.
// A group can live on more than one relay; the single-host door is gone,
// not deprecated.

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

    /// Watch the relay-signed records of every group matching `predicate`.
    /// One complete branch per host; each delivered element is the complete
    /// set of `NMPGroupSnapshot`s for the groups currently matching. The app
    /// never sees a row delta and never walks a `p` tag.
    ///
    /// `limit` is the ordinary NIP-01 filter limit and bounds EACH host's own
    /// branch, never the merged union: two hosts with `250` may deliver up to
    /// 500 snapshots, because each was asked for 250 of its own. `nil` asks
    /// for whatever the relay chooses to answer with.
    public func observeRecords(
        engine: NMPEngine,
        matching predicate: NMPGroupPredicate,
        records: [NMPGroupRecord],
        limit: UInt32? = nil
    ) throws -> NMPGroupRecordsObservation {
        let handle = try nmpRethrowing {
            try ffi.observeRecords(
                engine: engine.concreteFfiEngine,
                predicate: predicate.ffi,
                records: records.map { $0.toFfi() },
                limit: limit
            )
        }
        return NMPGroupRecordsObservation(handle: handle)
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

    /// Watch THIS group's own relay-signed records. Every delivered element
    /// is exactly one `NMPGroupSnapshot` -- this group's -- from the first
    /// delivery onward, including before any record has arrived, so there is
    /// always something to render.
    ///
    /// Not a second read door: it drains the Rust-owned handle over the one
    /// engine subscription the group's hosts declare.
    public func observeRecords(
        engine: NMPEngine,
        records: [NMPGroupRecord]
    ) throws -> NMPGroupObservation {
        let handle = try nmpRethrowing {
            try ffi.observeRecords(
                engine: engine.concreteFfiEngine,
                records: records.map { $0.toFfi() }
            )
        }
        return NMPGroupObservation(handle: handle)
    }

    /// Ask whether an already-signed event belongs to this group, without
    /// building a write out of it.
    public func validateContext(_ event: NMPSignedEvent) throws {
        try nmpRethrowing { try ffi.validateContext(event: event.toFfiSignedEvent()) }
    }

    /// Publish an unsigned draft into the group, as `authorPubkeyHex` -- the
    /// group's ONE write door (#1292).
    ///
    /// The `h` row is appended before signing, the route is the scope's own
    /// hosts, and `authorPubkeyHex` is frozen as an exact decoded hex pubkey
    /// rather than the active-account selector (#878). Returns the ORDINARY
    /// `Receipt`, store-issued `id` included.
    ///
    /// An app that needs a signed event WITHOUT publishing it asks the engine
    /// for exactly that: `NMPEngine.signEvent(...)` creates no write intent,
    /// receipt or publication and hands back the signed event.
    public func publish(
        engine: NMPEngine,
        authorPubkeyHex: String,
        kind: UInt16,
        tags: [[String]] = [],
        content: String = "",
        createdAt: UInt64? = nil
    ) throws -> Receipt {
        let receipts = try nmpRethrowing {
            try ffi.publish(
                engine: engine.concreteFfiEngine,
                author: authorPubkeyHex,
                builder: FfiEventBuilder(kind: kind, tags: tags, content: content, createdAt: createdAt)
            )
        }
        return Receipt(handle: receipts)
    }

    /// Publish a draft composed by the tagging door (#1243) into the group.
    ///
    /// The `h` row and the group's relay set stay this door's, exactly as for
    /// the field-by-field overload above: a composer owns the SCHEMA and the
    /// group owns the CONTEXT, and neither reaches into the other. What this
    /// adds is that a `chatReply(to:)` no longer has to be taken apart into
    /// kind/tags/content just to be published where it belongs.
    public func publish(
        engine: NMPEngine,
        authorPubkeyHex: String,
        payload: WritePayload
    ) throws -> Receipt {
        guard case .event(let kind, let tags, let content, let createdAt) = payload else {
            // A pre-signed event carries its own `h` already; the group door
            // contextualizes a draft and takes nothing else (#1292).
            throw NMPError.groupCallerSuppliedContext
        }
        return try publish(
            engine: engine, authorPubkeyHex: authorPubkeyHex, kind: kind, tags: tags,
            content: content, createdAt: createdAt)
    }

    /// kind:9021 -- ask to join. Publishable with no subscription at all.
    public func joinRequest(
        engine: NMPEngine,
        authorPubkeyHex: String,
        inviteCode: String? = nil
    ) throws -> Receipt {
        let receipts = try nmpRethrowing {
            try ffi.joinRequest(engine: engine.concreteFfiEngine, author: authorPubkeyHex, inviteCode: inviteCode)
        }
        return Receipt(handle: receipts)
    }

    /// kind:9022 -- leave.
    public func leaveRequest(
        engine: NMPEngine,
        authorPubkeyHex: String
    ) throws -> Receipt {
        let receipts = try nmpRethrowing {
            try ffi.leaveRequest(engine: engine.concreteFfiEngine, author: authorPubkeyHex)
        }
        return Receipt(handle: receipts)
    }

    /// kind:9000 -- add a member, optionally with a role.
    public func addUser(
        engine: NMPEngine,
        authorPubkeyHex: String,
        pubkeyHex: String,
        role: String? = nil
    ) throws -> Receipt {
        let receipts = try nmpRethrowing {
            try ffi.addUser(
                engine: engine.concreteFfiEngine, author: authorPubkeyHex, pubkey: pubkeyHex, role: role
            )
        }
        return Receipt(handle: receipts)
    }

    /// kind:9001 -- remove a member.
    public func removeUser(
        engine: NMPEngine,
        authorPubkeyHex: String,
        pubkeyHex: String
    ) throws -> Receipt {
        let receipts = try nmpRethrowing {
            try ffi.removeUser(engine: engine.concreteFfiEngine, author: authorPubkeyHex, pubkey: pubkeyHex)
        }
        return Receipt(handle: receipts)
    }

    /// kind:9002 -- state part of the group's metadata (#1282).
    ///
    /// Composes NIP-29's own 9002 rows and invents none: `name`, `about` and
    /// `picture`, plus the `public`/`private` and `open`/`closed` markers
    /// that decide who may read the group and whether join requests are
    /// honoured. An omitted field emits no tag, so it is left untouched
    /// rather than cleared.
    public func editMetadata(
        engine: NMPEngine,
        authorPubkeyHex: String,
        edit: NMPGroupMetadataEdit
    ) throws -> Receipt {
        let receipts = try nmpRethrowing {
            try ffi.editMetadata(
                engine: engine.concreteFfiEngine, author: authorPubkeyHex, edit: edit.toFfi()
            )
        }
        return Receipt(handle: receipts)
    }

    /// kind:9005 -- delete one group-hosted event.
    public func deleteEvent(
        engine: NMPEngine,
        authorPubkeyHex: String,
        eventID: String
    ) throws -> Receipt {
        let receipts = try nmpRethrowing {
            try ffi.deleteEvent(engine: engine.concreteFfiEngine, author: authorPubkeyHex, eventId: eventID)
        }
        return Receipt(handle: receipts)
    }

    /// kind:9007 -- create the group at its hosts.
    public func createGroup(
        engine: NMPEngine,
        authorPubkeyHex: String
    ) throws -> Receipt {
        let receipts = try nmpRethrowing {
            try ffi.createGroup(engine: engine.concreteFfiEngine, author: authorPubkeyHex)
        }
        return Receipt(handle: receipts)
    }

    /// kind:9008 -- delete the group from its hosts.
    public func deleteGroup(
        engine: NMPEngine,
        authorPubkeyHex: String
    ) throws -> Receipt {
        let receipts = try nmpRethrowing {
            try ffi.deleteGroup(engine: engine.concreteFfiEngine, author: authorPubkeyHex)
        }
        return Receipt(handle: receipts)
    }

    /// kind:9009 -- mint an invite code redeemable by `joinRequest`.
    public func createInvite(
        engine: NMPEngine,
        authorPubkeyHex: String,
        code: String
    ) throws -> Receipt {
        let receipts = try nmpRethrowing {
            try ffi.createInvite(engine: engine.concreteFfiEngine, author: authorPubkeyHex, code: code)
        }
        return Receipt(handle: receipts)
    }
}

/// Who may READ a group's messages (`nmp::nip29::ReadAccess` mirror, #1282).
///
/// NIP-29 spells the restricted state `["private"]`; the reference relay's
/// kind:9002 parser spells the permissive one `["public"]`, which is the only
/// way an edit can say "turn it back off".
public enum NMPReadAccess: Sendable, Hashable {
    /// `["public"]` -- anyone may read the group's messages.
    case `public`
    /// `["private"]` -- only members may read the group's messages.
    case `private`

    func toFfi() -> FfiReadAccess {
        switch self {
        case .public: return .public
        case .private: return .private
        }
    }
}

/// Whether JOIN REQUESTS are honoured (`nmp::nip29::JoinAccess` mirror,
/// #1282). Independent of `NMPReadAccess`: a group can be publicly readable
/// and still closed to new members.
public enum NMPJoinAccess: Sendable, Hashable {
    /// `["open"]` -- join requests are honoured.
    case open
    /// `["closed"]` -- join requests are ignored.
    case closed

    func toFfi() -> FfiJoinAccess {
        switch self {
        case .open: return .open
        case .closed: return .closed
        }
    }
}

/// What one kind:9002 edit says about a group
/// (`nmp::nip29::GroupMetadataEdit` mirror, #1282).
///
/// Every field is optional: `nil` leaves that row out of the draft entirely,
/// so it is not touched and never cleared. That is why the two markers are
/// two-valued enums rather than `Bool`s -- "make it public" and "do not
/// decide" are different statements, and one `Bool` cannot make both.
public struct NMPGroupMetadataEdit: Sendable, Hashable {
    /// The `name` row -- the group's display name.
    public var name: String?
    /// The `about` row -- the group's description.
    public var about: String?
    /// The `picture` row. The tag NAME is NIP-29's; which URL goes in it is
    /// entirely the app's product policy.
    public var picture: String?
    /// Who may read the group's messages.
    public var readAccess: NMPReadAccess?
    /// Whether join requests are honoured.
    public var joinAccess: NMPJoinAccess?

    public init(
        name: String? = nil,
        about: String? = nil,
        picture: String? = nil,
        readAccess: NMPReadAccess? = nil,
        joinAccess: NMPJoinAccess? = nil
    ) {
        self.name = name
        self.about = about
        self.picture = picture
        self.readAccess = readAccess
        self.joinAccess = joinAccess
    }

    func toFfi() -> FfiGroupMetadataEdit {
        FfiGroupMetadataEdit(
            name: name, about: about, picture: picture,
            readAccess: readAccess?.toFfi(), joinAccess: joinAccess?.toFfi())
    }
}

/// Which groups an observation covers (`nmp::nip29::GroupPredicate`/
/// `FfiGroupPredicate` mirror). Opaque by design -- built with `.all` or
/// `.naming(_:)`, then handed to
/// `NMPRelayScope.observeRecords(engine:matching:records:limit:)`.
///
/// Set algebra lives on `NMPGroupIds` and on nothing else, so
/// `.all.minus(...)` does not compile. Nostr filters have no negation, so
/// "everything except X" cannot narrow a wire request; an app that hides
/// muted rooms drops them from the snapshots it renders, where the cost is
/// visible.
public final class NMPGroupPredicate: @unchecked Sendable {
    let ffi: FfiGroupPredicate

    private init(_ ffi: FfiGroupPredicate) {
        self.ffi = ffi
    }

    /// Every group the host advertises among the selected records. The
    /// branch carries NO group-id row: this is the ABSENCE of a constraint,
    /// which is what makes a directory expressible -- the ids a directory
    /// wants are the answer, not the input.
    ///
    /// Unbounded by nature: bound it with `observeRecords`'s own `limit`.
    /// Advertisement is not enumeration -- a group the host serves but
    /// publishes no kind:39000 for is invisible.
    public static var all: NMPGroupPredicate {
        NMPGroupPredicate(FfiGroupPredicate.all())
    }

    /// Only the groups `ids` names.
    public static func naming(_ ids: NMPGroupIds) -> NMPGroupPredicate {
        NMPGroupPredicate(FfiGroupPredicate.naming(ids: ids.ffi))
    }
}

/// Where a set of NIP-29 group ids comes from (`nmp::nip29::GroupIds`/
/// `FfiGroupIds` mirror). Opaque by design -- built with
/// `.memberListIncludes`/`.adminListIncludes`/`.anyOf`/`.whoseRecordMatches`
/// and composed with `union`/`intersect`/`minus`.
///
/// Whatever this resolves to becomes the `#d` value set of one relay filter,
/// and a filter carrying very many values may be refused or silently
/// truncated by that relay. Watching very many groups needs sharding across
/// several observations; NMP does not chunk behind the app's back.
public final class NMPGroupIds: @unchecked Sendable {
    let ffi: FfiGroupIds

    private init(_ ffi: FfiGroupIds) {
        self.ffi = ffi
    }

    /// Groups whose own relay-signed record matches `selection` at the branch
    /// host -- THE general spelling, of which every leaf below is a
    /// shorthand. Throws when `selection` names no kind, or names a kind that
    /// is not one of NIP-29's three relay-signed group records: this leaf is
    /// evaluated with NIP-29's own pin, and a group host is authoritative for
    /// nothing else.
    public static func whoseRecordMatches(_ selection: NMPFilter) throws -> NMPGroupIds {
        try NMPGroupIds(
            nmpRethrowing { try NMPFFI.groupsWhoseRecordMatches(selection: selection.toFfi()) }
        )
    }

    /// Groups whose observed kind:39002 member-list evidence names
    /// `subjects`. Inclusion is evidence, never exact state -- absence is
    /// not evidence of non-membership.
    ///
    /// Shorthand for `.whoseRecordMatches({ kinds:[39002], #p: subjects })`.
    public static func memberListIncludes(_ subjects: NMPBinding) throws -> NMPGroupIds {
        try NMPGroupIds(
            nmpRethrowing { try NMPFFI.memberListIncludes(subjects: subjects.toFfi()) }
        )
    }

    /// Groups whose observed kind:39001 admin-list evidence names
    /// `subjects`. Evidence-scoped exactly like `memberListIncludes`.
    public static func adminListIncludes(_ subjects: NMPBinding) throws -> NMPGroupIds {
        try NMPGroupIds(
            nmpRethrowing { try NMPFFI.adminListIncludes(subjects: subjects.toFfi()) }
        )
    }

    /// The groups `ids` names, whatever any list says about them.
    ///
    /// `ids` is an ordinary `NMPBinding`: a literal set for rooms the app
    /// already knows, and a derived binding for rooms it has to look up.
    /// "Watch the groups named in my own kind:10009 simple-groups list" is
    /// that derived case, and it stays reactive -- when the list changes, the
    /// observation follows it. A derived binding keeps its OWN authority and
    /// is never repinned to the group's hosts.
    public static func anyOf(_ ids: NMPBinding) throws -> NMPGroupIds {
        try NMPGroupIds(nmpRethrowing { try NMPFFI.anyOf(ids: ids.toFfi()) })
    }

    /// Groups named by this source OR by any of `others`.
    public func union(_ others: [NMPGroupIds]) -> NMPGroupIds {
        NMPGroupIds(ffi.union(others: others.map { $0.ffi }))
    }

    /// Groups named by this source AND by all of `others`.
    public func intersect(_ others: [NMPGroupIds]) -> NMPGroupIds {
        NMPGroupIds(ffi.intersect(others: others.map { $0.ffi }))
    }

    /// Groups named by this source and by none of `others`.
    public func minus(_ others: [NMPGroupIds]) -> NMPGroupIds {
        NMPGroupIds(ffi.minus(others: others.map { $0.ffi }))
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

// ===========================================================================
// The relay-signed group records (#1233).
//
// Every value below is copied straight out of Rust. No `p`-tag walking, no
// role defaulting, no cross-host merge policy lives on this side of the
// boundary -- this file only mirrors Rust-owned state and drains Rust-owned
// handles, exactly like `Following.swift` does for NIP-02.
// ===========================================================================

/// Which of NIP-29's three relay-signed group records you are asking for
/// (`FfiGroupRecord` mirror).
public enum NMPGroupRecord: Sendable, Hashable {
    /// kind:39000 -- the group's own metadata.
    case metadata
    /// kind:39001 -- the optional, informative admin list.
    case admins
    /// kind:39002 -- the optional, possibly partial member list.
    case members

    init(_ ffi: FfiGroupRecord) {
        switch ffi {
        case .metadata: self = .metadata
        case .admins: self = .admins
        case .members: self = .members
        }
    }

    func toFfi() -> FfiGroupRecord {
        switch self {
        case .metadata: return .metadata
        case .admins: return .admins
        case .members: return .members
        }
    }
}

/// How much of what you asked for has been established (`FfiGroupAvailability`
/// mirror). It says nothing about whether the records are complete: a relay
/// that is `.ready` and published no member list has published no member list.
public enum NMPGroupAvailability: Sendable, Hashable {
    case sourceUnavailable
    case acquiring
    case cachedOnly
    case ready

    init(_ ffi: FfiGroupAvailability) {
        switch ffi {
        case .sourceUnavailable: self = .sourceUnavailable
        case .acquiring: self = .acquiring
        case .cachedOnly: self = .cachedOnly
        case .ready: self = .ready
        }
    }
}

/// One subject a relay-signed list names, and the hosts that named it
/// (`FfiListedSubject` mirror). `role` is `nil` when the relay wrote none --
/// never defaulted to "member".
public struct NMPListedSubject: Sendable, Hashable {
    public let pubkey: String
    public let role: String?
    public let hosts: [String]

    init(_ ffi: FfiListedSubject) {
        self.pubkey = ffi.pubkey
        self.role = ffi.role
        self.hosts = ffi.hosts
    }
}

/// One relay-signed list record (`FfiListedRecord` mirror).
public struct NMPListedRecord: Sendable, Hashable {
    public let subjects: [NMPListedSubject]
    /// The record's own `created_at`. A DISPLAY fact about this relay's
    /// record -- never compared against a local clock to adjudicate anything.
    public let asOf: UInt64
    public let eventID: String
    public let host: String

    init(_ ffi: FfiListedRecord) {
        self.subjects = ffi.subjects.map(NMPListedSubject.init)
        self.asOf = ffi.asOf
        self.eventID = ffi.eventId
        self.host = ffi.host
    }
}

/// One relay-signed kind:39000 record (`FfiGroupMetadata` mirror). The three
/// rows NIP-29 names are typed; `tags` carries the record's complete row list
/// verbatim, so reading a row NIP-29 core does not define (a `parent`, say)
/// needs no hand-parser here.
public struct NMPGroupMetadata: Sendable, Hashable {
    public let name: String?
    public let about: String?
    public let picture: String?
    public let tags: [[String]]
    public let asOf: UInt64
    public let eventID: String
    /// The relay that signed this record.
    public let host: String

    init(_ ffi: FfiGroupMetadata) {
        self.name = ffi.name
        self.about = ffi.about
        self.picture = ffi.picture
        self.tags = ffi.tags
        self.asOf = ffi.asOf
        self.eventID = ffi.eventId
        self.host = ffi.host
    }
}

/// Exactly what one host signed, folded with nothing (`FfiHostRecords`
/// mirror). Each record is optional because a relay genuinely may publish
/// one, two, or none of the three -- `nil` means "this host has published
/// none we have seen", never "there are none".
public struct NMPHostRecords: Sendable, Hashable {
    public let host: String
    public let metadata: NMPGroupMetadata?
    public let admins: NMPListedRecord?
    public let members: NMPListedRecord?
    public let availability: NMPGroupAvailability

    init(_ ffi: FfiHostRecords) {
        self.host = ffi.host
        self.metadata = ffi.metadata.map(NMPGroupMetadata.init)
        self.admins = ffi.admins.map(NMPListedRecord.init)
        self.members = ffi.members.map(NMPListedRecord.init)
        self.availability = NMPGroupAvailability(ffi.availability)
    }
}

/// One group, as the hosts currently describe it (`FfiGroupSnapshot` mirror).
/// A complete self-contained value, never a patch on a previous one.
public struct NMPGroupSnapshot: Sendable, Hashable {
    /// The `d` value the relay-signed records key themselves by.
    public let id: String
    /// The whole winning host's record -- latest `created_at` wins, never a
    /// field-wise merge. `metadata?.host` says which relay signed it.
    public let metadata: NMPGroupMetadata?
    /// The union across hosts, each entry carrying the hosts that named it.
    public let admins: [NMPListedSubject]
    public let members: [NMPListedSubject]
    /// The minimum over every host in the scope.
    public let availability: NMPGroupAvailability
    /// Exactly what each host that answered signed, in host order.
    public let perHost: [NMPHostRecords]

    private let disagreements: Set<NMPGroupRecord>

    init(_ ffi: FfiGroupSnapshot) {
        self.id = ffi.id
        self.metadata = ffi.metadata.map(NMPGroupMetadata.init)
        self.admins = ffi.admins.map(NMPListedSubject.init)
        self.members = ffi.members.map(NMPListedSubject.init)
        self.availability = NMPGroupAvailability(ffi.availability)
        self.perHost = ffi.perHost.map(NMPHostRecords.init)
        self.disagreements = Set(ffi.disagreements.map(NMPGroupRecord.init))
    }

    /// Exactly what `host` signed, or `nil` if it has published none of the
    /// selected records for this group that we have seen.
    public func at(_ host: String) -> NMPHostRecords? {
        perHost.first { $0.host == host }
    }

    /// Whether the hosts disagree about `record`, so a UI can decide whether
    /// a dig-in affordance is worth offering.
    public func differs(_ record: NMPGroupRecord) -> Bool {
        disagreements.contains(record)
    }
}

extension NmpGroupRecordsStream: NMPPullHandle {
    func pullNext() async throws -> [FfiGroupSnapshot]? { try await next() }
}

/// The relay-signed records of every group a predicate matches, as a
/// pull-based `AsyncSequence` (`NmpGroupRecordsStream` mirror). Each element
/// is the complete current snapshot set -- latest-wins, so no coalescer is
/// needed. Termination-tied teardown like `NMPQuery`; the handle is
/// single-consumer, so a second concurrent iterator surfaces
/// `NMPError.concurrentNext` rather than hanging.
public struct NMPGroupRecordsObservation: AsyncSequence, Sendable {
    public typealias Element = [NMPGroupSnapshot]

    private let handle: NmpGroupRecordsStream
    private let iteratorGate = NMPPullIteratorGate()

    init(handle: NmpGroupRecordsStream) {
        self.handle = handle
    }

    public func makeAsyncIterator() -> Iterator {
        let core = NMPPullIteratorCore(handle: handle, iteratorGate: iteratorGate) { snapshots in
            snapshots.map(NMPGroupSnapshot.init)
        }
        return Iterator(core: core)
    }

    public struct Iterator: AsyncIteratorProtocol {
        let core: NMPPullIteratorCore<NmpGroupRecordsStream, [NMPGroupSnapshot]>

        public mutating func next() async throws -> [NMPGroupSnapshot]? {
            try await core.next()
        }
    }

    /// Withdraw the observation now. Idempotent.
    public func cancel() {
        handle.cancel()
    }
}

/// One group's relay-signed records, as a pull-based `AsyncSequence` of the
/// single snapshot a group-scoped observation delivers. Same handle, same
/// teardown; it unwraps the one-element set so a room screen writes
/// `for try await room in ...` rather than indexing into an array it already
/// knows has exactly one element.
public struct NMPGroupObservation: AsyncSequence, Sendable {
    public typealias Element = NMPGroupSnapshot

    private let handle: NmpGroupRecordsStream
    private let iteratorGate = NMPPullIteratorGate()

    init(handle: NmpGroupRecordsStream) {
        self.handle = handle
    }

    public func makeAsyncIterator() -> Iterator {
        let core = NMPPullIteratorCore(handle: handle, iteratorGate: iteratorGate) { snapshots in
            snapshots.map(NMPGroupSnapshot.init)
        }
        return Iterator(core: core)
    }

    public struct Iterator: AsyncIteratorProtocol {
        let core: NMPPullIteratorCore<NmpGroupRecordsStream, [NMPGroupSnapshot]>

        public mutating func next() async throws -> NMPGroupSnapshot? {
            // A group-scoped observation delivers exactly one snapshot per
            // delivery. A delivery that somehow carried none is skipped
            // rather than ending the sequence: ending it would tear down a
            // live observation over a delivery that said nothing.
            while let snapshots = try await core.next() {
                if let only = snapshots.first { return only }
            }
            return nil
        }
    }

    /// Withdraw the observation now. Idempotent.
    public func cancel() {
        handle.cancel()
    }
}
