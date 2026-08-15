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

/// Source evidence for the live relationship projection. It does not gate
/// follow/unfollow: NMP can write cached state or create the first list while
/// relay truth is incomplete. `.ready` is not global Nostr completeness.
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
    public let currentPubkey: String?
    public let target: String
    public let relationship: NMPFollowRelationship
    public let availability: NMPFollowAvailability
    public let baseEventID: String?

    public init(
        currentPubkey: String?,
        target: String,
        relationship: NMPFollowRelationship,
        availability: NMPFollowAvailability,
        baseEventID: String?
    ) {
        self.currentPubkey = currentPubkey
        self.target = target
        self.relationship = relationship
        self.availability = availability
        self.baseEventID = baseEventID
    }

    init(_ ffi: FfiFollowSnapshot) {
        self.init(
            currentPubkey: ffi.currentPubkey,
            target: ffi.target,
            relationship: NMPFollowRelationship(ffi.relationship),
            availability: NMPFollowAvailability(ffi.availability),
            baseEventID: ffi.baseEventId
        )
    }

    public static func initial(target: String) -> Self {
        Self(
            currentPubkey: nil,
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
    case engineClosed
    case receiptUnavailable

    init(_ ffi: FfiFollowActionFailure) {
        switch ffi {
        case .invalidTarget(let got): self = .invalidTarget(got)
        case .signedOut: self = .signedOut
        case .engineClosed: self = .engineClosed
        case .receiptUnavailable: self = .receiptUnavailable
        }
    }
}

public enum NMPFollowActionStatus: Sendable, Hashable {
    case receipt(id: UInt64, status: WriteFact)
    case failed(NMPFollowActionFailure)

    init(_ ffi: FfiFollowActionStatus) {
        switch ffi {
        case .receipt(let receiptID, let status):
            self = .receipt(id: receiptID, status: WriteFact(status))
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

/// A thin pull-based projection of the ordinary write receipt created by one
/// typed follow/unfollow action. Successful actions contain only canonical
/// `WriteFact`s; immediate typed refusal is the sole non-receipt case.
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
    /// Observe whether the current account follows `target`. This is NMP's
    /// protocol projection, not an app-maintained boolean.
    public func observeFollowing(_ target: String) throws -> NMPFollowingObservation {
        try NMPFollowingObservation(engine: ffi, target: target)
    }

    /// Submit one durable NIP-02 operation. NMP applies it immediately to the
    /// best cached contact list, or to NIP-02's complete empty list when no
    /// source exists, and reapplies it if a newer relay source arrives.
    public func follow(_ target: String) throws -> NMPFollowAction {
        try NMPFollowAction(handle: nmpRethrowing {
            try ffi.follow(target: target)
        })
    }

    public func unfollow(_ target: String) throws -> NMPFollowAction {
        try NMPFollowAction(handle: nmpRethrowing {
            try ffi.unfollow(target: target)
        })
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
        snapshot.currentPubkey != nil
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

    /// The single action a connected control forwards. NMP retains and
    /// reapplies the operation; the UI owns no stale-base retry policy.
    public func performPrimaryAction() {
        guard canToggle else { return }
        toggle()
    }

    private func start(desiredFollowing: Bool) {
        guard !isActing else { return }
        let action: NMPFollowAction
        do {
            action = try (desiredFollowing ? engine.follow(target) : engine.unfollow(target))
        } catch {
            self.desiredFollowing = nil
            self.isActing = false
            return
        }
        self.desiredFollowing = desiredFollowing
        self.isActing = true
        self.actionStatus = nil
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
        case .failed:
            isActing = false
            desiredFollowing = nil
        case .receipt(_, let fact):
            if case .outcome(.refused) = fact {
                isActing = false
                desiredFollowing = nil
            } else if case .signing(.refused) = fact {
                isActing = false
                desiredFollowing = nil
            }
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
