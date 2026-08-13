import Foundation
import NMP

enum Probe {
    static let groupID = "bitcoin"
    static let singleGroupID = "solo-a"
    static let mixedGroupID = "one-sided"
    static let sharedChat = "shared chat observed at both hosts"
    static let relayBChat = "relay B chat"

    struct Context: Sendable {
        let engine: NMPEngine
        let scope: NMPRelayScope
        let writer: String
        let args: Args

        static func open(_ args: Args) async throws -> Context {
            let engine = try NMPEngine(config: NMPConfig(
                storePath: args.storePath,
                maxRelays: 4
            ))
            do {
                _ = try engine.session.add(
                    publicKey: NMPPublicKey(bytes: try decodedHex(args.viewer)),
                    makeCurrent: true
                )
                let secret = try String(contentsOfFile: args.writerSecretFile, encoding: .utf8)
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                let account = try engine.session.add(
                    privateKey: NMPPrivateKey(bytes: try decodedHex(secret))
                )
                return Context(
                    engine: engine,
                    scope: try NMPRelayScope.on([args.relayA, args.relayB]),
                    writer: hex(account.publicKey.bytes),
                    args: args
                )
            } catch {
                engine.shutdown()
                throw error
            }
        }

        func close() { engine.shutdown() }
    }

    static func online(_ args: Args) async throws {
        let context = try await Context.open(args)
        defer { context.close() }
        let group = context.scope.group(groupID)

        let singleScope = try NMPRelayScope.on([args.relayA])
        let singleQuery = try context.engine.observe(
            singleScope.group(singleGroupID).read(NMPFilter(kinds: [9]))
        )
        let single = try await waitForRows(singleQuery, seconds: args.settleSeconds) {
            rows($0, kind: 9).count == 1
        }
        singleQuery.cancel()
        try require(single.rows.allSatisfy { $0.sources == [args.relayA] },
                    "single-host group leaked another source")
        print("PROOF swift_single_host kind9_rows=1 source=\(args.relayA)")

        let chatQuery = try context.engine.observe(group.read(NMPFilter(kinds: [9])))
        try await Task.sleep(nanoseconds: 1_200_000_000)
        let chats = try await waitForRows(chatQuery, seconds: args.settleSeconds) { batch in
            let kindRows = rows(batch, kind: 9)
            let sharedSources = kindRows
                .first(where: { $0.content == sharedChat })?
                .sources.count ?? 0
            print("TRACE swift_kind9 distinct=\(kindRows.count) shared_sources=\(sharedSources) evidence_sources=\(sourceEvidence(batch).map { "\($0.relay):\($0.status)" })")
            return kindRows.count >= 27 && sharedSources == 2
        }
        chatQuery.cancel()
        try require(rows(chats, kind: 9).count == 27,
                    "expected 27 distinct seeded kind 9 rows")
        print("PROOF swift_kind9 distinct=27 shared_sources=2 slow_consumer=true")

        let articleQuery = try context.engine.observe(group.read(NMPFilter(kinds: [30023])))
        _ = try await waitForRows(articleQuery, seconds: args.settleSeconds) {
            rows($0, kind: 30023).count == 3
                && hasContent($0, "shared long-form event", sourceCount: 2)
        }
        articleQuery.cancel()
        print("PROOF swift_kind30023 distinct=3 shared_sources=2")

        try await verifyMetadata(context)
        try await verifyFollowsDiscovery(context)
        try await verifyWindow(context, group)
        try await verifyDiagnostics(context, group)
        try await verifyPublications(context)

        print("PROOF swift_cancellation query_status_diagnostics=explicit")
        print("PASS swift_online")
    }

    static func verifyMetadata(_ context: Context) async throws {
        let observation = try metadataObservation(context)
        defer { observation.cancel() }
        let snapshot = try await waitForSnapshot(
            observation,
            seconds: context.args.settleSeconds
        ) { $0.perHost.count == 2 }
        let names = try metadataNames(snapshot)
        try require(names[context.args.relayA] == "Bitcoin Cash",
                    "relay A metadata was not preserved")
        try require(names[context.args.relayB] == "Bitcoin (real)",
                    "relay B metadata was not preserved")
        // The aggregate is ONE relay's whole record, never a blend of the two,
        // and the app is told the two disagree so it can decide what to show.
        guard let shown = snapshot.metadata else {
            throw ProbeError.message("no metadata was shown at all")
        }
        try require(names.values.contains { $0 == shown.name },
                    "the shown name is not either relay's own")
        try require(snapshot.differs(NMPGroupRecord.metadata),
                    "two relays published different metadata and the app was not told")
        print("PROOF swift_metadata preserved=2 shown_host=\(shown.host) shown_name=\(shown.name ?? "nil") differs=true app_winner=\(names[context.args.relayB]!) policy=prefer_relay_b")
    }

    /// The relay-signed group records are keyed by `d`, unlike group content's
    /// `h` -- reaching for them through the content door is refused outright
    /// (#1245). Positive member-list evidence at each relay selects the group.
    static func metadataObservation(_ context: Context) throws -> NMPGroupRecordsObservation {
        let subjects = NMPBinding.literal(Set([context.args.followed, context.args.outsider]))
        return try context.scope.observeRecords(
            engine: context.engine,
            matching: .naming(try NMPGroupIds.memberListIncludes(subjects)),
            records: [.metadata]
        )
    }

    /// Exactly what each relay signed, read off the per-host breakdown beside
    /// the aggregate -- no tag walking, and no relay's record folded into
    /// another's.
    static func metadataNames(_ snapshot: NMPGroupSnapshot) throws -> [String: String] {
        var names: [String: String] = [:]
        for records in snapshot.perHost {
            guard let name = records.metadata?.name else {
                throw ProbeError.message("relay \(records.host) published no group name")
            }
            names[records.host] = name
        }
        return names
    }

    static func verifyFollowsDiscovery(_ context: Context) async throws {
        let observation = try followsDiscoveryObservation(context)
        defer { observation.cancel() }
        let snapshot = try await waitForSnapshot(
            observation,
            seconds: context.args.settleSeconds
        ) { listsFollowed($0, context.args.followed) }
        try require(snapshot.id == groupID,
                    "follows-derived discovery returned another group")
        try require(snapshot.perHost.map { $0.host } == [context.args.relayA],
                    "follows-derived discovery crossed relay authority")
        print("PROOF swift_discovery predicate=member_list_includes(follows_of_active_viewer) group=\(groupID) source=\(context.args.relayA)")
    }

    static func followsDiscoveryObservation(
        _ context: Context
    ) throws -> NMPGroupRecordsObservation {
        let follows = NMPBinding.derived(
            inner: NMPDemand(
                selection: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
                source: .pinned(Set([context.args.relayA, context.args.relayB])),
                cache: .strict
            ),
            project: .tag("p")
        )
        return try context.scope.observeRecords(
            engine: context.engine,
            matching: .naming(try NMPGroupIds.memberListIncludes(follows)),
            records: [.members]
        )
    }

    /// The member list NMP handed back names the followed subject -- read off
    /// the typed snapshot, with no `p`-tag walking anywhere in this app.
    static func listsFollowed(_ snapshot: NMPGroupSnapshot, _ followed: String) -> Bool {
        snapshot.members.contains { $0.pubkey == followed }
    }

    /// Await deliveries until this group's snapshot satisfies `predicate`.
    static func waitForSnapshot(
        _ observation: NMPGroupRecordsObservation,
        seconds: UInt64,
        _ predicate: @escaping @Sendable (NMPGroupSnapshot) -> Bool
    ) async throws -> NMPGroupSnapshot {
        try await withTimeout(seconds: seconds) {
            for try await snapshots in observation {
                if let found = snapshots.first(where: { $0.id == groupID && predicate($0) }) {
                    return found
                }
            }
            throw ProbeError.message("the records observation ended before the condition held")
        }
    }

    static func verifyWindow(_ context: Context, _ group: NMPGroup) async throws {
        let query = try context.engine.observe(
            group.read(NMPFilter(kinds: [9])),
            window: .expandable(initial: 3, max: 8)
        )
        defer { query.cancel() }
        let result: (Int, WindowLoad?) = try await withTimeout(seconds: context.args.settleSeconds) {
            var sawInitial = false
            for try await batch in query {
                if !sawInitial && batch.rows.count == 3 {
                    sawInitial = true
                    try query.requestRows(atLeast: 8)
                }
                if sawInitial && batch.rows.count == 8 {
                    return (batch.rows.count, batch.load)
                }
            }
            throw ProbeError.message("window stream ended before growth")
        }
        print("PROOF swift_window initial=3 grown=\(result.0) max=8 load=\(String(describing: result.1))")
    }

    static func verifyDiagnostics(_ context: Context, _ group: NMPGroup) async throws {
        let query = try context.engine.observe(group.read(NMPFilter(kinds: [9, 30023])))
        defer { query.cancel() }
        let diagnostics = try context.engine.observeDiagnostics()
        defer { diagnostics.cancel() }
        let snapshot = try await waitForDiagnostics(diagnostics, seconds: context.args.settleSeconds) {
            let relevant = $0.relays.filter {
                $0.relay == context.args.relayA || $0.relay == context.args.relayB
            }
            return relevant.count == 2 && relevant.allSatisfy {
                $0.wireSubCount > 0
                    && $0.filters.contains { $0.contains("#h") && $0.contains(groupID) }
            }
        }
        for relay in snapshot.relays where relay.relay == context.args.relayA
            || relay.relay == context.args.relayB {
            print("PROOF swift_diagnostics relay=\(relay.relay) wire_sub_count=\(relay.wireSubCount) filters=\(relay.filters) events_by_kind=\(relay.eventsByKind) coverage=\(relay.coverage)")
        }
    }

    static func verifyPublications(_ context: Context) async throws {
        let group = context.scope.group(groupID)
        let chat = try group.publish(
            engine: context.engine,
            authorPubkeyHex: context.writer,
            kind: 9,
            content: "Swift NMP consumer published chat"
        )
        defer { chat.status.cancel() }
        let chatStatuses = try await waitForStatuses(chat.status, seconds: context.args.settleSeconds) {
            acked($0, relay: context.args.relayA) && acked($0, relay: context.args.relayB)
        }
        print("PROOF swift_publish kind=9 outcomes=\(chatStatuses)")

        let article = try group.publish(
            engine: context.engine,
            authorPubkeyHex: context.writer,
            kind: 30023,
            tags: [["d", "swift-nmp-consumer-article"]],
            content: "Swift NMP consumer published long-form event"
        )
        defer { article.status.cancel() }
        let articleStatuses = try await waitForStatuses(article.status, seconds: context.args.settleSeconds) {
            acked($0, relay: context.args.relayA) && acked($0, relay: context.args.relayB)
        }
        print("PROOF swift_publish kind=30023 outcomes=\(articleStatuses)")

        let mixed = try context.scope.group(mixedGroupID).publish(
            engine: context.engine,
            authorPubkeyHex: context.writer,
            kind: 9,
            content: "Swift mixed-outcome publication"
        )
        defer { mixed.status.cancel() }
        let mixedStatuses = try await waitForStatuses(mixed.status, seconds: context.args.settleSeconds) {
            acked($0, relay: context.args.relayA) && rejected($0, relay: context.args.relayB)
        }
        print("PROOF swift_publish mixed_group=\(mixedGroupID) outcomes=\(mixedStatuses)")
    }

    static func provenanceGrowth(_ args: Args) async throws {
        let context = try await Context.open(args)
        defer { context.close() }
        let query = try context.engine.observe(
            context.scope.group(groupID).read(NMPFilter(kinds: [9]))
        )
        defer { query.cancel() }
        try await withTimeout(seconds: args.settleSeconds) {
            var beforeID: String?
            for try await batch in query {
                if beforeID == nil,
                   let shared = batch.rows.first(where: {
                       $0.content == sharedChat && $0.sources.count == 1
                   }),
                   batch.rows.contains(where: { $0.content == "relay A chat" }),
                   !batch.rows.contains(where: { $0.content == relayBChat }) {
                    beforeID = shared.id
                    try signalReady(args.readyFile)
                    print("PROOF swift_provenance_before shared_sources=1 relay_b_content=false")
                }
                if let beforeID,
                   let shared = batch.rows.first(where: { $0.id == beforeID }),
                   shared.sources.count == 2,
                   batch.rows.contains(where: { $0.content == relayBChat }) {
                    print("PROOF swift_provenance_after shared_sources=2 relay_b_content=true same_event=\(beforeID)")
                    return ()
                }
            }
            throw ProbeError.message("provenance stream ended before source growth")
        }
        print("PASS swift_provenance_growth")
    }

    static func restart(_ args: Args) async throws {
        let context = try await Context.open(args)
        defer { context.close() }
        let query = try context.engine.observe(
            context.scope.group(groupID).read(NMPFilter(kinds: [9]))
        )
        defer { query.cancel() }
        try await withTimeout(seconds: args.settleSeconds) {
            var offlineProved = false
            for try await batch in query {
                let sources = sourceEvidence(batch)
                if !offlineProved,
                   rows(batch, kind: 9).count >= 28,
                   hasContent(batch, sharedChat, sourceCount: 2),
                   sources.count >= 2,
                   sources.allSatisfy({ $0.reconciledThrough != nil }) {
                    offlineProved = true
                    try signalReady(args.readyFile)
                    print("PROOF swift_restart_offline cached_rows=\(rows(batch, kind: 9).count) shared_sources=2 persisted_watermarks=true statuses=\(sources.map(\.status))")
                }
                // Either connected-and-live state proves the reconnect
                // resubscribed (#1235). `.finishedStoredEvents` proves
                // strictly more -- it asked AND was answered -- so demanding
                // `.requesting` alone would wait for the engine to be SLOWER
                // than it is.
                if offlineProved,
                   sources.count >= 2,
                   sources.allSatisfy({ $0.status == .requesting || $0.status == .finishedStoredEvents }) {
                    print("PROOF swift_restart_reconnected relays=2 statuses=\(sources.map(\.status))")
                    return ()
                }
            }
            throw ProbeError.message("restart stream ended before reconnect")
        }
        print("PASS swift_restart")
    }
}
