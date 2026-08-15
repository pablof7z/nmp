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

/// A typed follow/unfollow action was refused before ordinary receipt
/// custody. `invalidTarget` is the one refusal this boundary adds: `target`
/// crosses FFI as a caller-typed hex string.
public enum FollowActionError: Error, Sendable, Equatable {
    case invalidTarget(got: String)
    case automaticRoutingUnavailable
    case signedOut
    case engineClosed
    case publishRefused(reason: String)

    init(_ ffi: FfiFollowActionError) {
        switch ffi {
        case .InvalidTarget(let got): self = .invalidTarget(got: got)
        case .AutomaticRoutingUnavailable: self = .automaticRoutingUnavailable
        case .SignedOut: self = .signedOut
        case .EngineClosed: self = .engineClosed
        case .PublishRefused(let reason): self = .publishRefused(reason: reason)
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

extension NMPEngine {
    /// Observe whether the current account follows `target`. This is NMP's
    /// protocol projection, not an app-maintained boolean.
    public func observeFollowing(_ target: String) throws -> NMPFollowingObservation {
        try NMPFollowingObservation(engine: ffi, target: target)
    }

    /// Ask NMP to follow `target` through the ordinary durable write and
    /// receipt lifecycle. NMP applies it immediately to the best cached
    /// contact list, or to NIP-02's complete empty list when no source
    /// exists, and reapplies it if a newer relay source arrives. Either a
    /// truthful immediate `FollowActionError`, or the same `Receipt` every
    /// other write returns.
    public func follow(_ target: String) throws -> Receipt {
        try followReceipt { try ffi.follow(target: target) }
    }

    /// The inverse of `follow(_:)`, with the same durable operation and
    /// ordinary receipt guarantees.
    public func unfollow(_ target: String) throws -> Receipt {
        try followReceipt { try ffi.unfollow(target: target) }
    }

    private func followReceipt(_ action: () throws -> NmpReceiptStream) throws -> Receipt {
        do {
            return Receipt(handle: try action())
        } catch let error as FfiFollowActionError {
            throw FollowActionError(error)
        }
    }
}

/// Bindable convenience over the two NMP APIs above. It owns no NIP-02
/// logic: snapshots and action statuses are copied directly from Rust; the
/// only local state is observation/task lifecycle for SwiftUI.
@MainActor
public final class NMPFollowing: ObservableObject {
    public let target: String

    @Published public private(set) var snapshot: NMPFollowingSnapshot
    @Published public private(set) var actionStatus: WriteFact?
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
        let receipt: Receipt
        do {
            receipt = try (desiredFollowing ? engine.follow(target) : engine.unfollow(target))
        } catch {
            // A truthful immediate refusal (FollowActionError): nothing
            // entered custody, so there is no stream to follow.
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
                for try await fact in receipt.status {
                    guard !Task.isCancelled else { return }
                    self?.accept(fact)
                }
            } catch {
                // The receipt stream ended abnormally; leave the last delivered
                // status in place (no capacity error exists to surface, #680).
            }
        }
    }

    private func accept(_ fact: WriteFact) {
        actionStatus = fact
        if case .outcome(.refused) = fact {
            isActing = false
            desiredFollowing = nil
        } else if case .signing(.refused) = fact {
            isActing = false
            desiredFollowing = nil
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
