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
    case cachedOnly
    case sourceUnavailable

    init(_ ffi: FfiFollowAvailability) {
        switch ffi {
        case .signedOut: self = .signedOut
        case .acquiring: self = .acquiring
        case .ready: self = .ready
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

/// Live relationship state over NMP's ordinary reactive kind:3 demand.
/// Latest-wins and deinit-tied, just like `NMPQuery`.
public struct NMPFollowingObservation: AsyncSequence, Sendable {
    public typealias Element = NMPFollowingSnapshot

    private let handle: NmpFollowHandle
    private let stream: AsyncStream<NMPFollowingSnapshot>

    init(engine: NmpEngineProtocol, target: String) throws {
        var continuation: AsyncStream<NMPFollowingSnapshot>.Continuation!
        let stream = AsyncStream<NMPFollowingSnapshot>(bufferingPolicy: .bufferingNewest(1)) {
            continuation = $0
        }
        let bridge = FollowingBridge(continuation: continuation)
        self.handle = try nmpRethrowing {
            try engine.observeFollowing(target: target, observer: bridge)
        }
        self.stream = stream
    }

    public func makeAsyncIterator() -> Iterator {
        Iterator(handle: handle, base: stream.makeAsyncIterator())
    }

    public struct Iterator: AsyncIteratorProtocol {
        private let handle: NmpFollowHandle
        private var base: AsyncStream<NMPFollowingSnapshot>.AsyncIterator

        init(
            handle: NmpFollowHandle,
            base: AsyncStream<NMPFollowingSnapshot>.AsyncIterator
        ) {
            self.handle = handle
            self.base = base
        }

        public mutating func next() async -> NMPFollowingSnapshot? {
            await base.next()
        }
    }

    public func cancel() {
        handle.cancel()
    }
}

private final class FollowingBridge: FollowObserver, @unchecked Sendable {
    private let continuation: AsyncStream<NMPFollowingSnapshot>.Continuation

    init(continuation: AsyncStream<NMPFollowingSnapshot>.Continuation) {
        self.continuation = continuation
    }

    func onSnapshot(snapshot: FfiFollowSnapshot) {
        continuation.yield(NMPFollowingSnapshot(snapshot))
    }

    func onClosed() {
        continuation.finish()
    }
}

public struct NMPFollowAction: Sendable {
    public let status: AsyncStream<NMPFollowActionStatus>
}

private final class FollowActionBridge: FollowActionObserver, @unchecked Sendable {
    private let continuation: AsyncStream<NMPFollowActionStatus>.Continuation

    init(continuation: AsyncStream<NMPFollowActionStatus>.Continuation) {
        self.continuation = continuation
    }

    func onStatus(status: FfiFollowActionStatus) {
        continuation.yield(NMPFollowActionStatus(status))
    }

    func onClosed() {
        continuation.finish()
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
        followingAction(target: target, follows: true)
    }

    public func unfollow(_ target: String) -> NMPFollowAction {
        followingAction(target: target, follows: false)
    }

    private func followingAction(target: String, follows: Bool) -> NMPFollowAction {
        var continuation: AsyncStream<NMPFollowActionStatus>.Continuation!
        let stream = AsyncStream<NMPFollowActionStatus> { continuation = $0 }
        let bridge = FollowActionBridge(continuation: continuation)
        if follows {
            ffi.follow(target: target, observer: bridge)
        } else {
            ffi.unfollow(target: target, observer: bridge)
        }
        return NMPFollowAction(status: stream)
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
            for await snapshot in observation {
                guard !Task.isCancelled else { return }
                self?.snapshot = snapshot
                self?.finishWhenCanonicalStateMatches(snapshot)
            }
        }
    }

    public var canToggle: Bool {
        snapshot.availability == .ready
            && snapshot.relationship != .unknown
            && !isActing
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

    private func start(desiredFollowing: Bool) {
        guard !isActing else { return }
        let action = desiredFollowing ? engine.follow(target) : engine.unfollow(target)
        self.desiredFollowing = desiredFollowing
        self.isActing = true
        self.actionStatus = .acquiring
        actionTask?.cancel()
        actionTask = Task { [weak self] in
            for await status in action.status {
                guard !Task.isCancelled else { return }
                self?.accept(status)
            }
        }
    }

    private func accept(_ status: NMPFollowActionStatus) {
        actionStatus = status
        switch status {
        case .noChange:
            isActing = false
            desiredFollowing = nil
        case .failed:
            isActing = false
            desiredFollowing = nil
        case .receipt(_, let status):
            if case .replaceableConflict = status {
                isActing = false
                desiredFollowing = nil
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
