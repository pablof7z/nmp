import Foundation
import NMP

private actor LiveAdversarialState {
    private var failure: String?
    private var metadataNames: [String: String] = [:]
    private var discoveryPresent = false
    private var cancelledChatReady = false
    private var survivingChatReady = false
    private var liveChatSources = 0

    func fail(_ message: String) {
        if failure == nil { failure = message }
    }

    func ended(_ stream: String) {
        if failure == nil { failure = "\(stream) stream ended unexpectedly" }
    }

    func ingestMetadata(_ batch: RowBatch) {
        var next: [String: String] = [:]
        for row in rows(batch, kind: 39000) {
            if let source = row.sources.first, let name = tagValue(row, "name") {
                next[source] = name
            }
        }
        metadataNames = next
    }

    func ingestDiscovery(_ batch: RowBatch, followed: String) {
        discoveryPresent = rows(batch, kind: 39002).contains { row in
            tagValue(row, "d") == Probe.groupID
                && row.tags.contains(["p", followed])
        }
    }

    func ingestChat(_ batch: RowBatch, survivor: Bool) {
        let ready = rows(batch, kind: 9).count >= 27
        if survivor {
            survivingChatReady = ready
            liveChatSources = rows(batch, kind: 9)
                .first(where: { $0.content == Probe.liveChat })?
                .sources.count ?? 0
        } else {
            cancelledChatReady = ready
        }
    }

    func initialReady(relayA: String, relayB: String) throws -> Bool {
        try checkFailure()
        return metadataNames[relayA] == "Bitcoin Cash"
            && metadataNames[relayB] == "Bitcoin (real)"
            && discoveryPresent
            && cancelledChatReady
            && survivingChatReady
    }

    func mutationReady(relayA: String, relayB: String) throws -> Bool {
        try checkFailure()
        return metadataNames[relayA] == "Bitcoin Cash live"
            && metadataNames[relayB] == "Bitcoin (real) live"
            && !discoveryPresent
            && liveChatSources == 2
    }

    func followRestored() throws -> Bool {
        try checkFailure()
        return discoveryPresent
    }

    func names() throws -> [String: String] {
        try checkFailure()
        return metadataNames
    }

    private func checkFailure() throws {
        if let failure { throw ProbeError.message(failure) }
    }
}

extension Probe {
    static let liveChat = "shared live chat after sibling cancellation"

    static func liveAdversarial(_ args: Args) async throws {
        let context = try await Context.open(args)
        defer { context.close() }
        let group = context.scope.group(groupID)
        let state = LiveAdversarialState()

        let subjects = NMPBinding.literal(Set([args.followed, args.outsider]))
        let metadataPredicate = try NMPGroupPredicate.memberListIncludes(subjects)
        let metadataQuery = try context.engine.observe(
            context.scope.groupsWhere(metadataPredicate)
        )
        let discoveryQuery = try context.engine.observe(
            context.scope.groupsWhere(try followsPredicate(context))
        )
        let chatDemand = group.read(NMPFilter(kinds: [9]))
        let cancelledQuery = try context.engine.observe(chatDemand)
        let survivingQuery = try context.engine.observe(chatDemand)

        let metadataTask = metadataPump(metadataQuery, state: state)
        let discoveryTask = discoveryPump(
            discoveryQuery,
            state: state,
            followed: args.followed
        )
        let cancelledTask = chatPump(cancelledQuery, state: state, survivor: false)
        let survivingTask = chatPump(survivingQuery, state: state, survivor: true)
        defer {
            metadataQuery.cancel()
            discoveryQuery.cancel()
            cancelledQuery.cancel()
            survivingQuery.cancel()
            metadataTask.cancel()
            discoveryTask.cancel()
            cancelledTask.cancel()
            survivingTask.cancel()
        }

        try await pollUntil(seconds: args.settleSeconds) {
            try await state.initialReady(relayA: args.relayA, relayB: args.relayB)
        }
        let sharedWire = try await waitForGroupFilterCounts(context, expected: 1)

        cancelledQuery.cancel()
        cancelledTask.cancel()
        let afterOneCancel = try await waitForGroupFilterCounts(context, expected: 1)
        try require(sharedWire == afterOneCancel,
                    "cancelling one shared observation changed surviving wire demand")

        try await stageRoundTrip(args, name: "mutate-live-inputs")
        try await pollUntil(seconds: args.settleSeconds) {
            try await state.mutationReady(relayA: args.relayA, relayB: args.relayB)
        }
        let liveNames = try await state.names()
        print("PROOF swift_live_mutation metadata=\(liveNames) follows_removed=true surviving_chat_sources=2 shared_wire=\(afterOneCancel)")

        try await stageRoundTrip(args, name: "restore-follow")
        try await pollUntil(seconds: args.settleSeconds) {
            try await state.followRestored()
        }
        print("PROOF swift_live_follow_readded group=\(groupID) observation_reused=true")

        survivingQuery.cancel()
        survivingTask.cancel()
        let afterLastCancel = try await waitForGroupFilterCounts(context, expected: 0)
        print("PROOF swift_shared_cancellation before=\(sharedWire) after_one=\(afterOneCancel) after_last=\(afterLastCancel)")
        print("PASS swift_live_adversarial")
    }

    static func restartConflict(_ args: Args) async throws {
        let context = try await Context.open(args)
        defer { context.close() }
        let subjects = NMPBinding.literal(Set([args.followed, args.outsider]))
        let predicate = try NMPGroupPredicate.memberListIncludes(subjects)
        let query = try context.engine.observe(context.scope.groupsWhere(predicate))
        defer { query.cancel() }

        try await withTimeout(seconds: args.settleSeconds) {
            var offlineProved = false
            for try await batch in query {
                let names = metadataNames(batch)
                let statuses = sourceStatuses(batch)
                if !offlineProved,
                   names[args.relayA] == "Bitcoin Cash live",
                   names[args.relayB] == "Bitcoin (real) live",
                   statuses[args.relayA] == .requesting,
                   statuses[args.relayB] != nil,
                   statuses[args.relayB] != .requesting {
                    offlineProved = true
                    try signalReady(args.readyFile)
                    print("PROOF swift_restart_conflict_offline metadata=\(names) cached_sources=2 statuses=\(statuses)")
                }
                if offlineProved, statuses[args.relayB] == .requesting {
                    print("PROOF swift_restart_conflict_reconnected metadata=\(names) statuses=\(statuses)")
                    return ()
                }
            }
            throw ProbeError.message("restart conflict stream ended before reconnect")
        }
        print("PASS swift_restart_conflict")
    }

    private static func followsPredicate(_ context: Context) throws -> NMPGroupPredicate {
        let follows = NMPBinding.derived(
            inner: NMPDemand(
                selection: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
                source: .pinned(Set([context.args.relayA, context.args.relayB])),
                cache: .strict
            ),
            project: .tag("p")
        )
        return try NMPGroupPredicate.memberListIncludes(follows)
    }

    private static func metadataPump(
        _ query: NMPQuery,
        state: LiveAdversarialState
    ) -> Task<Void, Never> {
        Task {
            do {
                for try await batch in query { await state.ingestMetadata(batch) }
                await state.ended("metadata")
            } catch { await state.fail("metadata stream failed: \(error)") }
        }
    }

    private static func discoveryPump(
        _ query: NMPQuery,
        state: LiveAdversarialState,
        followed: String
    ) -> Task<Void, Never> {
        Task {
            do {
                for try await batch in query {
                    await state.ingestDiscovery(batch, followed: followed)
                }
                await state.ended("discovery")
            } catch { await state.fail("discovery stream failed: \(error)") }
        }
    }

    private static func chatPump(
        _ query: NMPQuery,
        state: LiveAdversarialState,
        survivor: Bool
    ) -> Task<Void, Never> {
        Task {
            do {
                for try await batch in query {
                    await state.ingestChat(batch, survivor: survivor)
                }
            } catch {
                if !Task.isCancelled {
                    let name = survivor ? "surviving chat" : "cancelled chat"
                    await state.fail("\(name) stream failed: \(error)")
                }
            }
        }
    }

    private static func stageRoundTrip(_ args: Args, name: String) async throws {
        guard let stageDir = args.stageDir else {
            throw ProbeError.message("--stage-dir is required for live-adversarial")
        }
        try signalReady("\(stageDir)/\(name).ready")
        try await waitForFile("\(stageDir)/\(name).continue", seconds: args.settleSeconds)
    }

    private static func waitForGroupFilterCounts(
        _ context: Context,
        expected: Int
    ) async throws -> [String: Int] {
        let diagnostics = try context.engine.observeDiagnostics()
        defer { diagnostics.cancel() }
        let snapshot = try await waitForDiagnostics(
            diagnostics,
            seconds: context.args.settleSeconds
        ) { snapshot in
            let counts = groupFilterCounts(snapshot, context: context)
            return counts.count == 2 && counts.values.allSatisfy { $0 == expected }
        }
        return groupFilterCounts(snapshot, context: context)
    }

    private static func groupFilterCounts(
        _ snapshot: DiagnosticsSnapshot,
        context: Context
    ) -> [String: Int] {
        Dictionary(uniqueKeysWithValues: snapshot.relays.compactMap { relay in
            guard relay.relay == context.args.relayA || relay.relay == context.args.relayB else {
                return nil
            }
            let count = relay.filters.filter {
                $0.contains("\"kinds\":[9]")
                    && $0.contains("#h")
                    && $0.contains(groupID)
            }.count
            return (relay.relay, count)
        })
    }

    private static func metadataNames(_ batch: RowBatch) -> [String: String] {
        var names: [String: String] = [:]
        for row in rows(batch, kind: 39000) {
            if let source = row.sources.first, let name = tagValue(row, "name") {
                names[source] = name
            }
        }
        return names
    }

    private static func sourceStatuses(_ batch: RowBatch) -> [String: SourceStatus] {
        var statuses: [String: SourceStatus] = [:]
        for source in sourceEvidence(batch) { statuses[source.relay] = source.status }
        return statuses
    }
}
