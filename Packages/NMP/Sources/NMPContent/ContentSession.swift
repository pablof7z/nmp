import Combine
import Foundation
import NMP
import NMPFFI

/// Closed bounds for one root document's nested reference work.
public struct NostrContentPolicy: Sendable, Hashable {
    public var maxActiveReferences: Int
    public var maxResolvedReferences: Int
    public var maxDepth: Int
    public var releaseGraceMilliseconds: UInt64

    public init(
        maxActiveReferences: Int = 24,
        maxResolvedReferences: Int = 96,
        maxDepth: Int = 3,
        releaseGraceMilliseconds: UInt64 = 350
    ) {
        self.maxActiveReferences = max(1, maxActiveReferences)
        self.maxResolvedReferences = max(1, maxResolvedReferences)
        self.maxDepth = max(0, maxDepth)
        self.releaseGraceMilliseconds = releaseGraceMilliseconds
    }
}

/// Structural ancestry used to terminate recursive embeds independently of
/// SwiftUI view identity.
public struct NostrContentRenderContext: Sendable, Hashable {
    public let path: [String]
    public let depth: Int

    public static let root = NostrContentRenderContext(path: [], depth: 0)

    public init(path: [String], depth: Int) {
        self.path = path
        self.depth = max(0, depth)
    }

    public func descending(into targetKey: String) -> NostrContentRenderContext {
        NostrContentRenderContext(path: path + [targetKey], depth: depth + 1)
    }
}

/// Evidence remains separated by acquisition path. The runtime never turns a
/// relay-scoped shortfall into a global "not found" verdict.
public struct NostrContentEvidence: Sendable, Hashable {
    public let canonical: AcquisitionEvidence?
    public let helpers: [AcquisitionEvidence]

    public init(
        canonical: AcquisitionEvidence? = nil,
        helpers: [AcquisitionEvidence] = []
    ) {
        self.canonical = canonical
        self.helpers = helpers
    }
}

public enum NostrContentShortfall: Sendable, Hashable {
    case noPlannedSource
    case queryRejected(String)
    case invalidResolvedRow
}

public enum NostrContentCollapseReason: Sendable, Hashable {
    case cycle(targetKey: String)
    case depth(maximum: Int)
    case activeBudget(maximum: Int)
    case resolvedBudget(maximum: Int)
}

public enum NostrContentResource: Sendable, Hashable {
    case profile(metadata: NostrProfileMetadata, event: Row)
    case event(Row)

    public var event: Row {
        switch self {
        case .profile(_, let event), .event(let event): return event
        }
    }

    public var profile: NostrProfileMetadata? {
        guard case .profile(let metadata, _) = self else { return nil }
        return metadata
    }

    public var article: NostrArticle? {
        decodeNIP23Article(from: event)
    }
}

/// Latest state for either an occurrence or its deduplicated target.
public enum NostrReferenceState: Sendable, Hashable {
    case idle
    case loading(evidence: NostrContentEvidence)
    case refreshing(cached: NostrContentResource, evidence: NostrContentEvidence)
    case resolved(resource: NostrContentResource, evidence: NostrContentEvidence)
    /// The canonical live query retracted its row. The runtime does not claim
    /// why (deletion, expiration, or another scoped cause) without evidence.
    case withdrawn(previous: NostrContentResource, evidence: NostrContentEvidence)
    case shortfall(reason: NostrContentShortfall, evidence: NostrContentEvidence)
    case stopped(evidence: NostrContentEvidence)
    case collapsed(reason: NostrContentCollapseReason)

    public var resource: NostrContentResource? {
        switch self {
        case .resolved(let resource, _), .refreshing(let resource, _): return resource
        case .idle, .loading, .withdrawn, .shortfall, .stopped, .collapsed: return nil
        }
    }

    public var previousResource: NostrContentResource? {
        guard case .withdrawn(let previous, _) = self else { return nil }
        return previous
    }
}

/// One immutable latest-state delivery. Renderers read this synchronously and
/// never open their own NMP observations.
public struct NostrContentSnapshot: Sendable, Hashable {
    public let document: NostrContentDocument
    public let nodes: [UInt64: NostrReferenceState]
    public let resources: [String: NostrReferenceState]
    public let revision: UInt64
    public let activeReferenceCount: Int

    public init(
        document: NostrContentDocument,
        nodes: [UInt64: NostrReferenceState],
        resources: [String: NostrReferenceState],
        revision: UInt64,
        activeReferenceCount: Int
    ) {
        self.document = document
        self.nodes = nodes
        self.resources = resources
        self.revision = revision
        self.activeReferenceCount = activeReferenceCount
    }

    public func state(for occurrence: NostrReferenceOccurrence) -> NostrReferenceState {
        nodes[occurrence.id] ?? .idle
    }

    public func state(for target: NostrReferenceTarget) -> NostrReferenceState {
        resources[target.key] ?? .idle
    }
}

/// Optional facade over an existing engine. It owns no sockets, cache, relay
/// directory, or canonical winner logic.
public final class NMPContentClient: @unchecked Sendable {
    fileprivate let engine: NMPEngine

    public init(engine: NMPEngine) {
        self.engine = engine
    }

    @MainActor
    public func session(
        content: String,
        syntax: NostrContentSyntax = .plainText,
        policy: NostrContentPolicy = NostrContentPolicy(),
        context: NostrContentRenderContext = .root
    ) -> NostrContentSession {
        NostrContentSession(
            client: self,
            document: parseNostrContent(content, syntax: syntax),
            policy: policy,
            context: context
        )
    }

    @MainActor
    public func session(
        document: NostrContentDocument,
        policy: NostrContentPolicy = NostrContentPolicy(),
        context: NostrContentRenderContext = .root
    ) -> NostrContentSession {
        NostrContentSession(client: self, document: document, policy: policy, context: context)
    }
}

/// A deinit-safe claim on one target. SwiftUI primitives retain it while the
/// corresponding occurrence is render-relevant and release it on disappear.
public final class NostrContentClaim: @unchecked Sendable {
    private let lock = NSLock()
    private var didCancel = false
    private let release: @Sendable () -> Void

    fileprivate init(release: @escaping @Sendable () -> Void) {
        self.release = release
    }

    public func cancel() {
        lock.lock()
        guard !didCancel else {
            lock.unlock()
            return
        }
        didCancel = true
        lock.unlock()
        release()
    }

    deinit {
        cancel()
    }
}

@MainActor
public final class NostrContentSession: ObservableObject {
    public let policy: NostrContentPolicy
    public let context: NostrContentRenderContext

    /// `true` when claims lower into ordinary NMP live queries. Scripted
    /// sessions used by previews and deterministic state labs return `false`.
    public var isLive: Bool { liveClient != nil }

    @Published public private(set) var snapshot: NostrContentSnapshot

    private let liveClient: NMPContentClient?

    private struct TargetPlan {
        var target: NostrReferenceTarget
        var canonical: NMPDemand
        var helpers: [NMPDemand]
        var occurrenceIDs: Set<UInt64>
    }

    private var plans: [String: TargetPlan] = [:]
    private var targetForOccurrence: [UInt64: String] = [:]
    private var states: [String: NostrReferenceState] = [:]
    private var claimCounts: [String: Int] = [:]
    private var activeTargets: Set<String> = []
    private var waitingTargets: Set<String> = []
    private var resolvedTargets: Set<String> = []
    private var canonicalEvidence: [String: AcquisitionEvidence] = [:]
    private var helperEvidence: [String: [Int: AcquisitionEvidence]] = [:]
    private var observationTasks: [String: [Task<Void, Never>]] = [:]
    private var releaseTasks: [String: Task<Void, Never>] = [:]
    private var revision: UInt64 = 0

    public convenience init(
        client: NMPContentClient,
        document: NostrContentDocument,
        policy: NostrContentPolicy = NostrContentPolicy(),
        context: NostrContentRenderContext = .root
    ) {
        self.init(
            liveClient: client,
            document: document,
            scriptedResources: [:],
            policy: policy,
            context: context
        )
    }

    /// Build a deterministic content session without constructing an engine.
    /// Supplied states render synchronously, claims are inert, and no query or
    /// socket can be opened accidentally.
    public static func scripted(
        content: String,
        syntax: NostrContentSyntax = .plainText,
        resources: [NostrReferenceTarget: NostrReferenceState] = [:],
        policy: NostrContentPolicy = NostrContentPolicy(),
        context: NostrContentRenderContext = .root
    ) -> NostrContentSession {
        scripted(
            document: parseNostrContent(content, syntax: syntax),
            resources: resources,
            policy: policy,
            context: context
        )
    }

    /// Script an already-built semantic document. Custom Djot, AsciiDoc, or
    /// app-owned syntaxes can use the renderer without passing through NMP's
    /// plaintext/Markdown parser.
    public static func scripted(
        document: NostrContentDocument,
        resources: [NostrReferenceTarget: NostrReferenceState] = [:],
        policy: NostrContentPolicy = NostrContentPolicy(),
        context: NostrContentRenderContext = .root
    ) -> NostrContentSession {
        NostrContentSession(
            liveClient: nil,
            document: document,
            scriptedResources: Dictionary(
                uniqueKeysWithValues: resources.map { ($0.key.key, $0.value) }
            ),
            policy: policy,
            context: context
        )
    }

    private init(
        liveClient: NMPContentClient?,
        document: NostrContentDocument,
        scriptedResources: [String: NostrReferenceState],
        policy: NostrContentPolicy,
        context: NostrContentRenderContext
    ) {
        self.liveClient = liveClient
        self.policy = policy
        self.context = context
        self.snapshot = NostrContentSnapshot(
            document: document,
            nodes: [:],
            resources: [:],
            revision: 0,
            activeReferenceCount: 0
        )

        for occurrence in document.references {
            add(occurrence: occurrence)
        }
        for (targetKey, state) in scriptedResources {
            states[targetKey] = state
        }
        publishSnapshot(document: document)
    }

    /// Descend using the same acquisition mode as the parent. Live sessions
    /// reuse their client; scripted sessions stay network-free.
    public func nestedSession(
        content: String,
        syntax: NostrContentSyntax,
        context: NostrContentRenderContext
    ) -> NostrContentSession {
        if let liveClient {
            return liveClient.session(
                content: content,
                syntax: syntax,
                policy: policy,
                context: context
            )
        }
        return .scripted(
            content: content,
            syntax: syntax,
            policy: policy,
            context: context
        )
    }

    /// Claim one authored reference occurrence.
    @discardableResult
    public func claim(referenceID: UInt64) -> NostrContentClaim? {
        guard let key = targetForOccurrence[referenceID] else { return nil }
        return claim(targetKey: key)
    }

    /// Claim an arbitrary normalized target, useful for an event chrome's
    /// author profile or a standalone user card. Acquisition still flows
    /// through this session rather than through the leaf component.
    @discardableResult
    public func claim(target: NostrReferenceTarget) -> NostrContentClaim {
        let key = ensurePlan(for: target)
        return claim(targetKey: key)
    }

    @discardableResult
    public func claimProfile(pubkey: String) -> NostrContentClaim {
        claim(target: .profile(pubkey: pubkey))
    }

    public func state(for target: NostrReferenceTarget) -> NostrReferenceState {
        states[target.key] ?? .idle
    }

    /// Deterministically withdraw every content-derived demand now.
    public func stop() {
        guard liveClient != nil else { return }
        releaseTasks.values.forEach { $0.cancel() }
        releaseTasks.removeAll()
        observationTasks.values.flatMap { $0 }.forEach { $0.cancel() }
        observationTasks.removeAll()
        activeTargets.removeAll()
        waitingTargets.removeAll()
        for key in Array(states.keys) {
            guard let state = states[key] else { continue }
            switch state {
            case .loading, .shortfall, .stopped, .withdrawn:
                states[key] = .idle
            case .refreshing(let cached, let evidence):
                states[key] = .resolved(resource: cached, evidence: evidence)
            case .idle, .resolved, .collapsed:
                break
            }
        }
        publishSnapshot()
    }

    private func add(occurrence: NostrReferenceOccurrence) {
        let plan = referenceDemandPlan(for: occurrence.target)
        targetForOccurrence[occurrence.id] = plan.targetKey
        if var existing = plans[plan.targetKey] {
            existing.helpers.append(contentsOf: plan.helpers.filter { !existing.helpers.contains($0) })
            existing.occurrenceIDs.insert(occurrence.id)
            plans[plan.targetKey] = existing
        } else {
            plans[plan.targetKey] = TargetPlan(
                target: occurrence.target,
                canonical: plan.canonical,
                helpers: plan.helpers,
                occurrenceIDs: [occurrence.id]
            )
            states[plan.targetKey] = .idle
        }
    }

    private func ensurePlan(for target: NostrReferenceTarget) -> String {
        let plan = referenceDemandPlan(for: target)
        if var existing = plans[plan.targetKey] {
            existing.helpers.append(contentsOf: plan.helpers.filter { !existing.helpers.contains($0) })
            plans[plan.targetKey] = existing
        } else {
            plans[plan.targetKey] = TargetPlan(
                target: target,
                canonical: plan.canonical,
                helpers: plan.helpers,
                occurrenceIDs: []
            )
            states[plan.targetKey] = .idle
            publishSnapshot()
        }
        return plan.targetKey
    }

    private func claim(targetKey: String) -> NostrContentClaim {
        guard liveClient != nil else {
            return NostrContentClaim(release: {})
        }
        releaseTasks.removeValue(forKey: targetKey)?.cancel()
        claimCounts[targetKey, default: 0] += 1
        if claimCounts[targetKey] == 1 {
            startIfPossible(targetKey)
        }
        return NostrContentClaim { [weak self] in
            Task { @MainActor [weak self] in
                self?.release(targetKey: targetKey)
            }
        }
    }

    private func release(targetKey: String) {
        let remaining = max(0, (claimCounts[targetKey] ?? 0) - 1)
        claimCounts[targetKey] = remaining
        guard remaining == 0 else { return }

        let grace = policy.releaseGraceMilliseconds
        releaseTasks[targetKey] = Task { [weak self] in
            if grace > 0 {
                try? await Task.sleep(nanoseconds: grace * 1_000_000)
            }
            guard !Task.isCancelled else { return }
            self?.finishRelease(targetKey)
        }
    }

    private func finishRelease(_ targetKey: String) {
        guard claimCounts[targetKey] == 0 else { return }
        releaseTasks.removeValue(forKey: targetKey)
        waitingTargets.remove(targetKey)
        stopObserving(targetKey, preserveResolved: true)
        startWaitingTargets()
    }

    private func startIfPossible(_ targetKey: String) {
        guard plans[targetKey] != nil else { return }

        switch evaluateContentClaim(
            targetKey: targetKey,
            path: context.path,
            depth: UInt8(clamping: context.depth),
            activeReferences: UInt32(clamping: activeTargets.count),
            policy: policy.ffiValue
        ) {
        case .acquire:
            startObserving(targetKey)
        case .cycle(let cycleKey):
            states[targetKey] = .collapsed(reason: .cycle(targetKey: cycleKey))
            publishSnapshot()
        case .depthLimit(let maximum):
            states[targetKey] = .collapsed(reason: .depth(maximum: Int(maximum)))
            publishSnapshot()
        case .activeLimit(let maximum):
            waitingTargets.insert(targetKey)
            states[targetKey] = .collapsed(
                reason: .activeBudget(maximum: Int(maximum))
            )
            publishSnapshot()
        }
    }

    private func startWaitingTargets() {
        guard activeTargets.count < policy.maxActiveReferences else { return }
        let candidates = waitingTargets.sorted()
        for key in candidates where activeTargets.count < policy.maxActiveReferences {
            guard (claimCounts[key] ?? 0) > 0 else {
                waitingTargets.remove(key)
                continue
            }
            waitingTargets.remove(key)
            startObserving(key)
        }
    }

    private func startObserving(_ targetKey: String) {
        guard let liveClient,
              !activeTargets.contains(targetKey),
              let plan = plans[targetKey]
        else { return }
        activeTargets.insert(targetKey)
        if let cached = states[targetKey]?.resource {
            states[targetKey] = .refreshing(cached: cached, evidence: evidence(for: targetKey))
        } else {
            states[targetKey] = .loading(evidence: evidence(for: targetKey))
        }
        publishSnapshot()

        var tasks: [Task<Void, Never>] = []
        do {
            let canonical = try liveClient.engine.observe(plan.canonical)
            tasks.append(Task { [weak self] in
                defer { canonical.cancel() }
                do {
                    for try await batch in canonical {
                        guard !Task.isCancelled else { break }
                        self?.receiveCanonical(batch, targetKey: targetKey)
                    }
                } catch {
                    // The observation ended (withdrawal / single-consumer
                    // misuse); fall through to the stopped transition (#680).
                }
                guard !Task.isCancelled else { return }
                self?.canonicalStopped(targetKey)
            })

            for (index, demand) in plan.helpers.enumerated() {
                let helper = try liveClient.engine.observe(demand)
                tasks.append(Task { [weak self] in
                    defer { helper.cancel() }
                    do {
                        for try await batch in helper {
                            guard !Task.isCancelled else { break }
                            self?.receiveHelper(batch, targetKey: targetKey, index: index)
                        }
                    } catch {
                        // The observation ended; nothing further to deliver (#680).
                    }
                })
            }
            observationTasks[targetKey] = tasks
        } catch {
            tasks.forEach { $0.cancel() }
            activeTargets.remove(targetKey)
            states[targetKey] = .shortfall(
                reason: .queryRejected(String(describing: error)),
                evidence: evidence(for: targetKey)
            )
            publishSnapshot()
            startWaitingTargets()
        }
    }

    private func receiveCanonical(_ batch: RowBatch, targetKey: String) {
        guard activeTargets.contains(targetKey), let plan = plans[targetKey] else { return }
        canonicalEvidence[targetKey] = batch.evidence
        guard let row = batch.rows.first else {
            if let previous = states[targetKey]?.resource {
                states[targetKey] = .withdrawn(
                    previous: previous,
                    evidence: evidence(for: targetKey)
                )
            } else if batch.evidence.shortfall.isEmpty {
                states[targetKey] = .loading(evidence: evidence(for: targetKey))
            } else {
                states[targetKey] = .shortfall(
                    reason: .noPlannedSource,
                    evidence: evidence(for: targetKey)
                )
            }
            publishSnapshot()
            return
        }

        if case .resolvedLimit(let maximum) = evaluateContentResolution(
            targetAlreadyResolved: resolvedTargets.contains(targetKey),
            resolvedReferences: UInt32(clamping: resolvedTargets.count),
            policy: policy.ffiValue
        ) {
            states[targetKey] = .collapsed(
                reason: .resolvedBudget(maximum: Int(maximum))
            )
            stopObserving(targetKey, preserveResolved: false)
            publishSnapshot()
            startWaitingTargets()
            return
        }

        let resource: NostrContentResource?
        switch plan.target {
        case .profile:
            resource = decodeNostrProfile(from: row).map { .profile(metadata: $0, event: row) }
        case .event, .address:
            resource = .event(row)
        }

        guard let resource else {
            states[targetKey] = .shortfall(
                reason: .invalidResolvedRow,
                evidence: evidence(for: targetKey)
            )
            publishSnapshot()
            return
        }
        resolvedTargets.insert(targetKey)
        states[targetKey] = .resolved(resource: resource, evidence: evidence(for: targetKey))
        publishSnapshot()
    }

    private func receiveHelper(_ batch: RowBatch, targetKey: String, index: Int) {
        helperEvidence[targetKey, default: [:]][index] = batch.evidence
        if states[targetKey]?.resource == nil {
            states[targetKey] = batch.evidence.shortfall.isEmpty
                ? .loading(evidence: evidence(for: targetKey))
                : .shortfall(reason: .noPlannedSource, evidence: evidence(for: targetKey))
        } else if let resource = states[targetKey]?.resource {
            if case .refreshing = states[targetKey] {
                states[targetKey] = .refreshing(
                    cached: resource,
                    evidence: evidence(for: targetKey)
                )
            } else {
                states[targetKey] = .resolved(
                    resource: resource,
                    evidence: evidence(for: targetKey)
                )
            }
        }
        publishSnapshot()
    }

    private func canonicalStopped(_ targetKey: String) {
        guard activeTargets.contains(targetKey) else { return }
        if states[targetKey]?.resource == nil {
            states[targetKey] = .stopped(evidence: evidence(for: targetKey))
            publishSnapshot()
        }
    }

    private func evidence(for targetKey: String) -> NostrContentEvidence {
        NostrContentEvidence(
            canonical: canonicalEvidence[targetKey],
            helpers: helperEvidence[targetKey, default: [:]]
                .sorted { $0.key < $1.key }
                .map(\.value)
        )
    }

    private func stopObserving(_ targetKey: String, preserveResolved: Bool) {
        observationTasks.removeValue(forKey: targetKey)?.forEach { $0.cancel() }
        activeTargets.remove(targetKey)
        if preserveResolved, case .refreshing(let cached, let evidence) = states[targetKey] {
            states[targetKey] = .resolved(resource: cached, evidence: evidence)
        } else if !preserveResolved || states[targetKey]?.resource == nil {
            states[targetKey] = .idle
        }
        publishSnapshot()
    }

    private func publishSnapshot(document: NostrContentDocument? = nil) {
        revision &+= 1
        let document = document ?? snapshot.document
        var nodes: [UInt64: NostrReferenceState] = [:]
        for (occurrenceID, targetKey) in targetForOccurrence {
            nodes[occurrenceID] = states[targetKey] ?? .idle
        }
        snapshot = NostrContentSnapshot(
            document: document,
            nodes: nodes,
            resources: states,
            revision: revision,
            activeReferenceCount: activeTargets.count
        )
    }

    deinit {
        releaseTasks.values.forEach { $0.cancel() }
        observationTasks.values.flatMap { $0 }.forEach { $0.cancel() }
    }
}

private extension NostrContentPolicy {
    var ffiValue: FfiContentHydrationPolicy {
        FfiContentHydrationPolicy(
            maxActiveReferences: UInt32(clamping: maxActiveReferences),
            maxResolvedReferences: UInt32(clamping: maxResolvedReferences),
            maxDepth: UInt8(clamping: maxDepth)
        )
    }
}
