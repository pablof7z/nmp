import Combine
import NMPFFI

public enum NMPFollowRelationship: Sendable, Hashable {
    case unknown
    case notFollowing
    case following

    init(_ ffi: FfiFollowRelationship) {
        switch ffi {
        case .unknown: self = .unknown
        case .notFollowing: self = .notFollowing
        case .following: self = .following
        }
    }
}

/// Whether NMP's NIP-02 action can safely compose a whole-list replacement
/// from the current source-scoped snapshot. `.ready` is explicitly about
/// every source in the current plan; it is not a claim that Nostr is
/// globally complete.
public enum NMPFollowAvailability: Sendable, Hashable {
    case signedOut
    case acquiring
    case ready
    case noContactList
    case cachedOnly
    case sourceUnavailable

    init(_ ffi: FfiFollowAvailability) {
        switch ffi {
        case .signedOut: self = .signedOut
        case .acquiring: self = .acquiring
        case .ready: self = .ready
        case .noContactList: self = .noContactList
        case .cachedOnly: self = .cachedOnly
        case .sourceUnavailable: self = .sourceUnavailable
        }
    }
}

public struct NMPFollowingSnapshot: Sendable, Hashable {
    public let activePubkey: String?
    public let target: String
    public let relationship: NMPFollowRelationship
    public let availability: NMPFollowAvailability
    public let baseEventID: String?

    public init(
        activePubkey: String?,
        target: String,
        relationship: NMPFollowRelationship,
        availability: NMPFollowAvailability,
        baseEventID: String?
    ) {
        self.activePubkey = activePubkey
        self.target = target
        self.relationship = relationship
        self.availability = availability
        self.baseEventID = baseEventID
    }

    init(_ ffi: FfiFollowSnapshot) {
        self.init(
            activePubkey: ffi.activePubkey,
            target: ffi.target,
            relationship: NMPFollowRelationship(ffi.relationship),
            availability: NMPFollowAvailability(ffi.availability),
            baseEventID: ffi.baseEventId
        )
    }

    public static func initial(target: String) -> Self {
        Self(
            activePubkey: nil,
            target: target,
            relationship: .unknown,
            availability: .acquiring,
            baseEventID: nil
        )
    }
}

public enum NMPFollowActionFailure: Sendable, Hashable {
    case invalidTarget(String)
    case signedOut
    case accountChanged
    case acquisitionTimedOut
    case noContactList
    case cachedOnly
    case sourceUnavailable
    case baseHasWrongAuthor
    case baseHasWrongKind
    case timestampExhausted
    case invalidGeneratedTag
    case engineClosed
    case receiptUnavailable

    init(_ ffi: FfiFollowActionFailure) {
        switch ffi {
        case .invalidTarget(let got): self = .invalidTarget(got)
        case .signedOut: self = .signedOut
        case .accountChanged: self = .accountChanged
        case .acquisitionTimedOut: self = .acquisitionTimedOut
        case .noContactList: self = .noContactList
        case .cachedOnly: self = .cachedOnly
        case .sourceUnavailable: self = .sourceUnavailable
        case .baseHasWrongAuthor: self = .baseHasWrongAuthor
        case .baseHasWrongKind: self = .baseHasWrongKind
        case .timestampExhausted: self = .timestampExhausted
        case .invalidGeneratedTag: self = .invalidGeneratedTag
        case .engineClosed: self = .engineClosed
        case .receiptUnavailable: self = .receiptUnavailable
        }
    }
}

public enum NMPFollowActionStatus: Sendable, Hashable {
    case acquiring
    case noChange(following: Bool)
    case receipt(id: UInt64, status: WriteStatus)
    case failed(NMPFollowActionFailure)

    init(_ ffi: FfiFollowActionStatus) {
        switch ffi {
        case .acquiring:
            self = .acquiring
        case .noChange(let following):
            self = .noChange(following: following)
        case .receipt(let receiptID, let status):
            self = .receipt(id: receiptID, status: WriteStatus(status))
        case .failed(let failure):
            self = .failed(NMPFollowActionFailure(failure))
        }
    }
}

/// Live relationship state over NMP's ordinary reactive kind:3 demand
/// (#680). A pull-based `AsyncSequence` over `NmpFollowStream` -- each
/// snapshot is the complete self-contained relationship state (latest-wins),
/// so no coalescer is needed. Termination-tied teardown like `NMPQuery`.
public struct NMPFollowingObservation: AsyncSequence, Sendable {
    public typealias Element = NMPFollowingSnapshot

    private let handle: NmpFollowStream
    private let iteratorGate = NMPPullIteratorGate()

    init(engine: NmpEngineProtocol, target: String) throws {
        self.handle = try nmpRethrowing {
            try engine.observeFollowing(target: target)
        }
    }

    public func makeAsyncIterator() -> Iterator {
        let core = NMPPullIteratorCore(handle: handle, iteratorGate: iteratorGate) { snapshot in
            NMPFollowingSnapshot(snapshot)
        }
        return Iterator(core: core)
    }

    public struct Iterator: AsyncIteratorProtocol {
        let core: NMPPullIteratorCore<NmpFollowStream, NMPFollowingSnapshot>

        public mutating func next() async throws -> NMPFollowingSnapshot? {
            try await core.next()
        }
    }

    public func cancel() {
        handle.cancel()
    }
}

/// The NMP-owned follow/unfollow action as a pull-based `AsyncSequence` over
/// `NmpFollowActionStream` (#680). Statuses are FIFO facts (acquisition,
/// no-op, atomic conflict, signing, routing, relay receipt), so they are
/// delivered un-coalesced and un-buffered-drop, in order.
public struct NMPFollowAction: AsyncSequence, Sendable {
    public typealias Element = NMPFollowActionStatus

    private let handle: NmpFollowActionStream
    private let iteratorGate = NMPPullIteratorGate()

    init(handle: NmpFollowActionStream) {
        self.handle = handle
    }

    public func makeAsyncIterator() -> Iterator {
        let core = NMPPullIteratorCore(handle: handle, iteratorGate: iteratorGate) { status in
            NMPFollowActionStatus(status)
        }
        return Iterator(core: core)
    }

    public struct Iterator: AsyncIteratorProtocol {
        let core: NMPPullIteratorCore<NmpFollowActionStream, NMPFollowActionStatus>

        public mutating func next() async throws -> NMPFollowActionStatus? {
            try await core.next()
        }
    }

    public func cancel() {
        handle.cancel()
    }
}

extension NMPEngine {
    /// Observe whether the active account follows `target`. This is NMP's
    /// protocol projection, not an app-maintained boolean.
    public func observeFollowing(_ target: String) throws -> NMPFollowingObservation {
        try NMPFollowingObservation(engine: ffi, target: target)
    }

    /// The simple NMP-owned follow action. It returns immediately with a
    /// stream covering acquisition, no-op, atomic conflict, signing,
    /// routing, and relay receipt states.
    public func follow(_ target: String) -> NMPFollowAction {
        NMPFollowAction(handle: ffi.follow(target: target))
    }

    public func unfollow(_ target: String) -> NMPFollowAction {
        NMPFollowAction(handle: ffi.unfollow(target: target))
    }
}

/// Bindable convenience over the two NMP APIs above. It owns no NIP-02
/// logic: snapshots and action statuses are copied directly from Rust; the
/// only local state is observation/task lifecycle for SwiftUI.
@MainActor
public final class NMPFollowing: ObservableObject {
    public let target: String

    @Published public private(set) var snapshot: NMPFollowingSnapshot
    @Published public private(set) var actionStatus: NMPFollowActionStatus?
    @Published public private(set) var isActing = false

    private let engine: NMPEngine
    private var desiredFollowing: Bool?
    private nonisolated(unsafe) var observationTask: Task<Void, Never>?
    private nonisolated(unsafe) var actionTask: Task<Void, Never>?

    public init(engine: NMPEngine, target: String) throws {
        self.engine = engine
        self.target = target
        self.snapshot = .initial(target: target)
        let observation = try engine.observeFollowing(target)
        observationTask = Task { [weak self] in
            do {
                for try await snapshot in observation {
                    guard !Task.isCancelled else { return }
                    self?.snapshot = snapshot
                    self?.finishWhenCanonicalStateMatches(snapshot)
                }
            } catch {
                // The observation ended (withdrawal / single-consumer misuse);
                // stop updating. NMP surfaces no capacity error here (#680).
            }
        }
    }

    public var canToggle: Bool {
        snapshot.availability == .ready
            && snapshot.relationship != .unknown
            && !isActing
    }

    /// Presentation state derived from NMP's typed action stream. This is
    /// intent pending an explicit second tap, never optimistic follow truth.
    public var offersAnotherAttempt: Bool {
        guard desiredFollowing != nil, let actionStatus else { return false }
        switch actionStatus {
        case .failed(.acquisitionTimedOut),
             .failed(.cachedOnly),
             .failed(.sourceUnavailable),
             .receipt(_, .replaceableConflict):
            return true
        default:
            return false
        }
    }

    public func follow() {
        start(desiredFollowing: true)
    }

    public func unfollow() {
        start(desiredFollowing: false)
    }

    public func toggle() {
        guard canToggle else { return }
        switch snapshot.relationship {
        case .following: unfollow()
        case .notFollowing: follow()
        case .unknown: break
        }
    }

    /// The single action a connected control forwards. Retry policy remains
    /// beside the NMP action/resource rather than inside a SwiftUI view.
    public func performPrimaryAction() {
        guard canToggle else { return }
        if offersAnotherAttempt, let desiredFollowing {
            start(desiredFollowing: desiredFollowing)
        } else {
            toggle()
        }
    }

    private func start(desiredFollowing: Bool) {
        guard !isActing else { return }
        let action = desiredFollowing ? engine.follow(target) : engine.unfollow(target)
        self.desiredFollowing = desiredFollowing
        self.isActing = true
        self.actionStatus = .acquiring
        actionTask?.cancel()
        actionTask = Task { [weak self] in
            do {
                for try await status in action {
                    guard !Task.isCancelled else { return }
                    self?.accept(status)
                }
            } catch {
                // The action stream ended abnormally; leave the last delivered
                // status in place (no capacity error exists to surface, #680).
            }
        }
    }

    private func accept(_ status: NMPFollowActionStatus) {
        actionStatus = status
        switch status {
        case .noChange:
            isActing = false
            desiredFollowing = nil
        case .failed(.acquisitionTimedOut),
             .failed(.cachedOnly),
             .failed(.sourceUnavailable):
            isActing = false
        case .failed:
            isActing = false
            desiredFollowing = nil
        case .receipt(_, let status):
            if case .replaceableConflict = status {
                isActing = false
            } else if case .failed = status {
                isActing = false
                desiredFollowing = nil
            }
        case .acquiring:
            break
        }
    }

    private func finishWhenCanonicalStateMatches(_ snapshot: NMPFollowingSnapshot) {
        guard let desiredFollowing else { return }
        let matches = desiredFollowing
            ? snapshot.relationship == .following
            : snapshot.relationship == .notFollowing
        if matches {
            isActing = false
            self.desiredFollowing = nil
        }
    }

    deinit {
        observationTask?.cancel()
        actionTask?.cancel()
    }
}
