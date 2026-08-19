// C11 (docs/internals/canary.md "Scenario status"): a CAPABILITY works end
// to end through the app, against a real strfry child process. The
// capability is NIP-22 comments, and the two halves of the claim are that
// its write composes into NMP's ordinary write noun and that its read
// composes into NMP's ordinary read noun -- nothing capability-shaped in
// the lifecycle, nothing capability-shaped in the transport.
//
// WHY THIS SCENARIO IS NAMED WHAT IT IS (#1875). C11 was listed as
// "semantic capability", a phrase that appeared exactly once in the whole
// repository -- in the line that listed it -- with no definition, no
// matching public API and no protocol category by that name, so the slot was
// renamed to what it means and the name was taken from the public surface: "capability" is
// `docs/internals/crate-architecture.md` rule 2's own word (a capability
// owns its meaning in its own crate) and `nmp-ffi`'s own feature keys, and
// "NIP-22 comments" is the protocol's name and the name of
// `Packages/NMP/Sources/NMP/NIP22.swift`.
//
// WHY NIP-22 AND NOT NIP-29. NIP-29 has the larger Swift surface
// (`NIP29.swift`, `NIP29SimpleGroups.swift`) but its group records --
// kind:39000/39001/39002 metadata, admins, members -- are RELAY-GENERATED.
// strfry is an ordinary storage relay with no NIP-29 implementation at all,
// so `createGroup` there publishes an event nothing acts on and
// `observeRecords` would report `.unavailable` forever. Proving NIP-29 end
// to end needs a NIP-29 relay in the lab, which is a different piece of
// work. NIP-22 comments are ordinary events any relay stores, so the
// capability -- not the relay's feature set -- is what is under test here.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C7/C9/C13/C17.
// Public `NMP` API only: plain `import NMP`, no `@testable`, no internal
// crate, no direct Redb inspection. The relay is reached only over a real
// `ws://` URL to a separate process.
//
// TWO ENGINES, TWO STORES, ONE RELAY. The author app publishes; the reader
// app has its own `NMPEngine` over its own store path and was never told
// anything about the write. The ONLY path from one to the other is the
// relay, deterministically and by construction -- so "the reader saw it"
// cannot be satisfied by a row that never left the writer's own store,
// which is the vacuity a single-engine version of this scenario would have.
//
// THE PRECONDITION IS ASSERTED BEFORE THE BEHAVIOUR, twice over per write,
// because a scenario that never actually sent anything reads green. Before
// any "the thread came back" assertion runs, each write must have
// (1) reached `RelayState.published` for this exact relay on its own
// `receipt.status`, and (2) been handed back by the relay itself over an
// independent real `REQ` by id (`RelayLabKit.queryById`, a plain socket
// with no NMP involvement). Neither is the correctness oracle -- the app's
// own queries are -- but without both, everything below would be asserted
// against a write that never landed.
//
// Every wait is a bounded poll on a real condition that reports the real
// stuck values on timeout, never a fixed sleep used AS the oracle.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C11CommentCapabilityEndToEndTests: XCTestCase {
    // MARK: - What one observation has been delivered

    /// One query's delivered state at a point in time. `rows` is the newest
    /// batch (an `NMPQuery` element is the whole current snapshot, not a
    /// delta); `everSeen` is the union across every batch, because "the row
    /// arrived" and "the row stayed" are different questions.
    private struct LedgerSnapshot: Sendable {
        var rows: [Row] = []
        var everSeen: Set<String> = []
        var batches = 0
        var duplicateWitness: String?
        var ended: String?

        var ids: Set<String> { Set(rows.map(\.id)) }
        func has(_ id: String) -> Bool { rows.contains { $0.id == id } }
        func row(_ id: String) -> Row? { rows.first { $0.id == id } }

        var report: String {
            "\(rows.count) rows \(ids.sorted().map { String($0.prefix(8)) }) over \(batches) batches"
                + (duplicateWitness.map { ", DUPLICATE IN ONE BATCH: \($0)" } ?? "")
                + (ended.map { ", ended: \($0)" } ?? "")
        }
    }

    private actor RowLedger {
        private var state = LedgerSnapshot()

        func record(_ batch: RowBatch) {
            state.batches += 1
            let ids = batch.rows.map(\.id)
            if Set(ids).count != ids.count {
                state.duplicateWitness = ids.joined(separator: " ")
            }
            state.rows = batch.rows
            state.everSeen.formUnion(ids)
        }

        func markEnded(_ why: String) { state.ended = why }

        func snapshot() -> LedgerSnapshot { state }
    }

    /// Bounded poll on a real condition. Returns whether it ever held; the
    /// caller reports the real stuck values on `false`. The sleep paces the
    /// poll -- it is never the thing being waited on.
    @discardableResult
    private func waitUntil(
        timeout: TimeInterval = 30,
        _ condition: () async -> Bool
    ) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if await condition() { return true }
            try? await Task.sleep(nanoseconds: 25_000_000)
        }
        return await condition()
    }

    /// What one write's receipt stream reported. Consumed to a bounded
    /// deadline rather than to termination: `.published` for this relay is
    /// the precondition, and whatever else arrived is reported too so a
    /// failure names the whole picture rather than one bit of it.
    private struct WriteEvidence: Sendable {
        var signedEventID: String?
        var publishedHere = false
        var relayStates: [String] = []
        var outcome: String?

        var report: String {
            "id=\(signedEventID.map { String($0.prefix(8)) } ?? "nil") publishedHere=\(publishedHere) "
                + "relayStates=\(relayStates) outcome=\(outcome ?? "none yet")"
        }
    }

    /// Accumulates what the receipt stream has said so far, so a TIMEOUT
    /// still reports the real facts that did arrive rather than an empty
    /// record. The first falsification run below is what exposed the need:
    /// routing the write to a dead address reported `relayStates=[]` with no
    /// signed id, which is true but says nothing about how far the write
    /// actually got.
    private actor EvidenceBox {
        private var evidence = WriteEvidence()

        func noteSigned(_ eventID: String) { evidence.signedEventID = eventID }
        func noteRelayState(_ label: String) { evidence.relayStates.append(label) }
        func notePublished(_ eventID: String) {
            if evidence.signedEventID == nil { evidence.signedEventID = eventID }
            evidence.publishedHere = true
        }
        func noteOutcome(_ label: String) { evidence.outcome = label }
        func current() -> WriteEvidence { evidence }
    }

    /// Drive one receipt to the point where THIS relay reports `.published`,
    /// bounded. Shared by the writes below: it is a wait loop, not an
    /// abstraction over the API -- every public type it touches
    /// (`ReceiptStatus`, `WriteFact`, `RelayState`, `SigningState`) is named
    /// in the open right here.
    private func awaitPublished(
        _ receipt: Receipt,
        at relayURL: String,
        timeout: TimeInterval = 30
    ) async -> WriteEvidence {
        let box = EvidenceBox()
        await withTaskGroup(of: Void.self) { group in
            group.addTask {
                do {
                    for try await fact in receipt.status {
                        switch fact {
                        case .signing(let state):
                            if case .signed(let eventID) = state { await box.noteSigned(eventID) }
                        case .relay(let eventID, let relay, let state):
                            guard relay == relayURL else { continue }
                            await box.noteRelayState("\(state)")
                            if case .published = state {
                                await box.notePublished(eventID)
                                return
                            }
                        case .outcome(let outcome):
                            await box.noteOutcome("\(outcome)")
                            return
                        case .destinations:
                            continue
                        }
                    }
                } catch {
                    await box.noteOutcome("receipt stream threw: \(error)")
                }
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: UInt64(timeout * 1_000_000_000))
            }
            await group.next()
            group.cancelAll()
        }
        return await box.current()
    }

    // MARK: - C11: the capability, end to end, into a second app

    func testANip22CommentThreadRoundTripsThroughARealRelayIntoASecondApp() async throws {
        let binaryPath = try Self.locateStrfryBinary()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c11-\(UUID().uuidString)")
        let relayDir = root.appendingPathComponent("relay")
        let authorStoreDir = root.appendingPathComponent("author-store")
        let readerStoreDir = root.appendingPathComponent("reader-store")
        for dir in [relayDir, authorStoreDir, readerStoreDir] {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        }
        defer { try? FileManager.default.removeItem(at: root) }

        let relay = try await RelayHandle(name: "c11-relay", workDir: relayDir, binaryPath: binaryPath)
        try await relay.start()

        // Two ordinary apps. Separate engines, separate Redb stores, one
        // relay between them -- the only channel either has to the other.
        let authorEngine = try NMPEngine(
            config: NMPConfig(
                storePath: authorStoreDir.appendingPathComponent("nmp.redb").path,
                appRelays: [relay.url]
            )
        )
        defer { authorEngine.shutdown() }
        let readerEngine = try NMPEngine(
            config: NMPConfig(
                storePath: readerStoreDir.appendingPathComponent("nmp.redb").path,
                appRelays: [relay.url]
            )
        )
        defer { readerEngine.shutdown() }

        let authorAccount = try authorEngine.session.add(
            privateKey: NMPPrivateKey.generate(), makeCurrent: true
        )
        let readerAccount = try readerEngine.session.add(
            privateKey: NMPPrivateKey.generate(), makeCurrent: true
        )
        let authorHex = Self.hex(authorAccount.publicKey.bytes)
        let readerHex = Self.hex(readerAccount.publicKey.bytes)

        // The thread's root is a page on the web (NIP-73), so the thread
        // exists before any Nostr event does and the scenario needs no
        // seeded root event to have a thread at all. A fresh URL per run
        // means every kind:1111 event in this thread is unambiguously one
        // this scenario wrote.
        let page = "https://example.com/canary-c11/\(UUID().uuidString.lowercased())"
        let threadRoot = CommentRoot.external(target: .url(url: page))

        // The capability's OWN read door, opened on both apps BEFORE
        // anything is written -- a query opened afterwards would prove the
        // engine can start a subscription and find history, not that a live
        // thread delivers.
        let authorThreadQuery = try authorEngine.observe(.single(commentThreadDemand(root: threadRoot)))
        let readerThreadQuery = try readerEngine.observe(.single(commentThreadDemand(root: threadRoot)))
        // ...and one ORDINARY query that knows nothing about NIP-22: a bare
        // kind + authors filter, the same `observe(_ filter:)` every other
        // scenario uses. A capability's writes must be ordinary events in
        // the ordinary store, readable without the capability.
        let readerPlainQuery = try readerEngine.observe(
            .single(NMPDemand(selection: NMPFilter(kinds: [1111], authors: .literal([authorHex, readerHex]))))
        )

        let authorThread = RowLedger()
        let readerThread = RowLedger()
        let readerPlain = RowLedger()

        let consumers = Task {
            await withTaskGroup(of: Void.self) { group in
                group.addTask {
                    do {
                        for try await batch in authorThreadQuery { await authorThread.record(batch) }
                        await authorThread.markEnded("sequence ended")
                    } catch { await authorThread.markEnded("threw: \(error)") }
                }
                group.addTask {
                    do {
                        for try await batch in readerThreadQuery { await readerThread.record(batch) }
                        await readerThread.markEnded("sequence ended")
                    } catch { await readerThread.markEnded("threw: \(error)") }
                }
                group.addTask {
                    do {
                        for try await batch in readerPlainQuery { await readerPlain.record(batch) }
                        await readerPlain.markEnded("sequence ended")
                    } catch { await readerPlain.markEnded("threw: \(error)") }
                }
            }
        }
        defer { consumers.cancel() }

        var log: [String] = []
        func note(_ line: String) { log.append(line) }
        defer { print((["", "C11 phase log:"] + log).joined(separator: "\n")) }

        // --- Step 1: the capability composes an ORDINARY write ------------

        let topLevelText = "C11: a comment on a web page"
        var topLevelIntent = try NMP.commentIntent(on: .root(threadRoot), content: topLevelText)
        // `commentIntent` returns `WriteRouting.auto`, and `.auto` requires
        // `NMPConfig.outboxRouting` indexers this engine deliberately does
        // not have -- the same read/write routing asymmetry
        // docs/internals/canary.md already records as an ergonomics finding.
        // Stating the destination is a plain assignment to a public field on
        // the ordinary write noun, exactly as C7 does; nothing
        // capability-shaped is involved.
        topLevelIntent.routing = .explicit(relays: [relay.url])
        XCTAssertEqual(
            topLevelIntent.identity, .active,
            "the capability composes for the ACTIVE account -- it must not name an author itself"
        )
        let topLevelReceipt = try await authorEngine.publish(topLevelIntent)

        // --- Step 2: PRECONDITION -- it really reached the relay ----------

        let topLevelEvidence = await awaitPublished(topLevelReceipt, at: relay.url)
        note("top-level publish: \(topLevelEvidence.report)")
        XCTAssertTrue(
            topLevelEvidence.publishedHere,
            "PRECONDITION: the composed comment never reached RelayState.published at \(relay.url) "
                + "-- \(topLevelEvidence.report). Everything below would be asserted against a "
                + "write that never landed."
        )
        guard let topLevelID = topLevelEvidence.signedEventID else {
            return XCTFail("PRECONDITION: no signed event id on the receipt -- \(topLevelEvidence.report)")
        }
        // The second, NMP-independent proof: the relay itself hands the
        // event back over a plain REQ on a fresh socket.
        let topLevelOnRelay = try await relay.queryById(topLevelID)
        note("top-level on relay by REQ: \(topLevelOnRelay != nil) kind=\(topLevelOnRelay?["kind"] ?? "nil")")
        XCTAssertNotNil(
            topLevelOnRelay,
            "PRECONDITION: a real REQ for \(topLevelID) returned nothing from \(relay.url), so the "
                + "relay does not hold the comment and no read below can honestly be about it"
        )
        XCTAssertEqual(
            topLevelOnRelay?["kind"] as? Int, 1111,
            "the capability must have written a kind:1111 comment"
        )

        // --- Step 3: the SECOND app reads the thread back -----------------
        //
        // Through the capability's own demand, on an engine that has never
        // been told this event exists and shares nothing with the writer
        // except the relay.

        let readerSawTopLevel = await waitUntil(timeout: 30) {
            await readerThread.snapshot().has(topLevelID)
        }
        let readerAfterTopLevel = await readerThread.snapshot()
        note("reader thread query after the top-level comment: \(readerAfterTopLevel.report)")
        XCTAssertTrue(
            readerSawTopLevel,
            "the second app's commentThreadDemand never delivered \(topLevelID) in 30s -- "
                + "\(readerAfterTopLevel.report). The comment is provably on the relay (asserted "
                + "above), so this is the capability's read door failing, not the write."
        )
        guard let topLevelAsReaderSawIt = readerAfterTopLevel.row(topLevelID) else {
            return XCTFail("the delivered batch no longer holds \(topLevelID)")
        }
        // It came over the network: the reader engine never wrote it, so the
        // relay must be named in its provenance.
        XCTAssertTrue(
            topLevelAsReaderSawIt.sources.contains(relay.url),
            "the second app's row does not name \(relay.url) in its sources "
                + "(\(topLevelAsReaderSawIt.sources)) -- it did not come from the relay"
        )

        // --- Step 4: reply to it, composed off the row NMP delivered ------
        //
        // `CommentTarget.row` is the door whose whole point is that the
        // thread's root is read off the target's OWN rows rather than
        // restated by a caller. The reader never states the root here.

        let replyText = "C11: a reply to that comment"
        var replyIntent = try NMP.commentIntent(on: .row(topLevelAsReaderSawIt), content: replyText)
        replyIntent.routing = .explicit(relays: [relay.url])
        let replyReceipt = try await readerEngine.publish(replyIntent)

        let replyEvidence = await awaitPublished(replyReceipt, at: relay.url)
        note("reply publish: \(replyEvidence.report)")
        XCTAssertTrue(
            replyEvidence.publishedHere,
            "PRECONDITION: the reply never reached RelayState.published at \(relay.url) -- "
                + "\(replyEvidence.report)"
        )
        guard let replyID = replyEvidence.signedEventID else {
            return XCTFail("PRECONDITION: no signed event id for the reply -- \(replyEvidence.report)")
        }
        let replyOnRelay = try await relay.queryById(replyID)
        note("reply on relay by REQ: \(replyOnRelay != nil)")
        XCTAssertNotNil(
            replyOnRelay,
            "PRECONDITION: a real REQ for the reply \(replyID) returned nothing from \(relay.url)"
        )

        // --- Step 5: ONE demand covers the WHOLE thread -------------------
        //
        // The capability's headline claim, and the only reason its demand
        // exists rather than a hand-built filter: one filter carries
        // top-level comments AND every reply, at any depth. Neither app
        // reopened anything -- the queries opened in step 0 must now hold
        // both.

        let authorSawBoth = await waitUntil(timeout: 30) {
            let snapshot = await authorThread.snapshot()
            return snapshot.has(topLevelID) && snapshot.has(replyID)
        }
        let readerSawBoth = await waitUntil(timeout: 30) {
            let snapshot = await readerThread.snapshot()
            return snapshot.has(topLevelID) && snapshot.has(replyID)
        }
        let plainSawBoth = await waitUntil(timeout: 30) {
            let snapshot = await readerPlain.snapshot()
            return snapshot.has(topLevelID) && snapshot.has(replyID)
        }
        let authorFinal = await authorThread.snapshot()
        let readerFinal = await readerThread.snapshot()
        let plainFinal = await readerPlain.snapshot()
        note("author thread query: \(authorFinal.report)")
        note("reader thread query: \(readerFinal.report)")
        note("reader plain query:  \(plainFinal.report)")
        note("thread ids: top-level=\(topLevelID) reply=\(replyID)")

        XCTAssertTrue(
            authorSawBoth,
            "one commentThreadDemand must cover the whole thread: the author app's still-open "
                + "thread query holds \(authorFinal.report), missing the reply \(replyID) which is "
                + "provably on the relay"
        )
        XCTAssertTrue(
            readerSawBoth,
            "the reader app's thread query holds \(readerFinal.report)"
        )
        // The capability composed ordinary events: a filter that has never
        // heard of NIP-22 finds exactly the same two rows.
        XCTAssertTrue(
            plainSawBoth,
            "an ORDINARY NMPFilter(kinds: [1111], authors: ...) must find what the capability "
                + "wrote -- it holds \(plainFinal.report)"
        )
        XCTAssertEqual(
            authorFinal.ids, [topLevelID, replyID],
            "the author's thread query must hold exactly the two events of this thread, got "
                + "\(authorFinal.report)"
        )
        XCTAssertEqual(
            readerFinal.ids, [topLevelID, replyID],
            "the reader's thread query must hold exactly the two events of this thread, got "
                + "\(readerFinal.report)"
        )
        XCTAssertEqual(
            plainFinal.ids, [topLevelID, replyID],
            "the ordinary query must hold exactly the two events of this thread, got "
                + "\(plainFinal.report)"
        )
        XCTAssertEqual(
            authorFinal.rows.count, 2,
            "the author's thread query delivered \(authorFinal.rows.count) rows against 2 event ids "
                + "-- above the id count this is a duplicate canonical row, below it a lost event"
        )
        XCTAssertNil(
            authorFinal.duplicateWitness,
            "the author's thread query was delivered a batch carrying one id twice: "
                + "\(authorFinal.duplicateWitness ?? "")"
        )
        XCTAssertNil(
            readerFinal.duplicateWitness,
            "the reader's thread query was delivered a batch carrying one id twice: "
                + "\(readerFinal.duplicateWitness ?? "")"
        )

        // --- Step 6: what came back DECODES as what was composed ----------
        //
        // Round trip closed at the capability's own typed layer, on the
        // author app's view -- where the REPLY arrived over the network
        // rather than from local acceptance.

        guard let topLevelRow = authorFinal.row(topLevelID),
              let replyRow = authorFinal.row(replyID)
        else {
            return XCTFail("the author's latest batch no longer holds both rows: \(authorFinal.report)")
        }
        XCTAssertTrue(
            replyRow.sources.contains(relay.url),
            "the author app's copy of the reply does not name \(relay.url) in its sources "
                + "(\(replyRow.sources)) -- it did not arrive over the network"
        )

        let decodedTopLevel = try decodeComment(topLevelRow)
        let decodedReply = try decodeComment(replyRow)
        note("decoded top-level: root=\(decodedTopLevel.root) parent=\(decodedTopLevel.parent)")
        note("decoded reply:     root=\(decodedReply.root) parent=\(decodedReply.parent)")

        XCTAssertEqual(decodedTopLevel.content, topLevelText)
        XCTAssertEqual(decodedTopLevel.authorPubkey, authorHex)
        // The decoded root must REOPEN the thread it was composed on. This
        // is the app-meaningful claim, and it is asserted through the
        // capability's own demand rather than through `==` on the root, for
        // a reason worth stating in full:
        //
        // A `Nip73.url` composes into `["I", <url>], ["K", "web"]` and
        // DECODES BACK as `Nip73.general(value: <url>, kind: "web")` -- the
        // decoder deliberately never re-canonicalises a read, and `.url`'s
        // whole meaning is "already canonicalised". So the round trip does
        // not preserve the typed variant, `decodedTopLevel.root !=
        // threadRoot` even though both name one page, and `Nip73` is
        // `Hashable`, so an app keying comments by their root splits one
        // thread across two keys. Recorded rather than asserted either way
        // (#1878): the value is printed on every run so neither shape is
        // silently promoted to a contract.
        note("composed root: \(threadRoot)")
        note("decoded root == composed root: \(decodedTopLevel.root == threadRoot) (see #1878)")
        XCTAssertEqual(
            try commentThreadDemand(root: decodedTopLevel.root),
            try commentThreadDemand(root: threadRoot),
            "the decoded root must reopen the same thread it was composed on -- an app that "
                + "decodes a comment and asks for its thread must not get a different thread"
        )
        XCTAssertEqual(
            decodedTopLevel.parent, .root,
            "a comment composed on the thread root is a TOP-LEVEL comment"
        )

        XCTAssertEqual(decodedReply.content, replyText)
        XCTAssertEqual(decodedReply.authorPubkey, readerHex)
        // The property that makes one filter enough: every comment in a
        // thread carries the IDENTICAL root, whatever its depth.
        XCTAssertEqual(
            decodedReply.root, decodedTopLevel.root,
            "a reply must carry the identical root as the comment it replies to -- that identity "
                + "is the only reason one demand can cover a whole thread"
        )
        // ...while the parent is what varies with depth, and the reply's
        // parent was never stated by the app: it was read off the delivered
        // row by `CommentTarget.row`.
        XCTAssertEqual(
            decodedReply.parent, .comment(eventID: topLevelID, authorPubkey: authorHex),
            "the reply's parent must be the comment it was composed on, with its author"
        )

        consumers.cancel()
        try await relay.kill()
    }

    // MARK: - The finding, as an executable falsifier (#1876)
    //
    // KNOWN RED, ON PURPOSE. `commentThreadDemand` binds the root
    // identifier to the `#I` tag for EVERY root shape, but the composer
    // writes `E` for an event root and `A` for an address root -- `I` only
    // for a NIP-73 external one. So the app-shaped case, commenting on a
    // note, composes and publishes perfectly and can then never be read
    // back through the capability's own door.
    //
    // This test asserts the CORRECT behaviour, not the current one: it goes
    // green with no edit the moment #1876 lands. It is deliberately not
    // inverted into an assertion that the bug is right, because that would
    // report the fix as a regression. Same discipline as C17's `distinct`
    // phase, which is also left red rather than have its threshold raised.
    //
    // It also proves the failure is the DEMAND and nothing else: the same
    // comment, on the same relay, in the same run, IS delivered to an
    // ordinary `NMPFilter` scoped on the `E` tag by hand -- which is the
    // workaround, and which requires the app to know NIP-22's
    // uppercase-root tag shape, the exact knowledge the capability crate
    // exists to own.

    func testAnEventRootedCommentThreadIsDeliveredThroughItsOwnDemand() async throws {
        let binaryPath = try Self.locateStrfryBinary()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c11e-\(UUID().uuidString)")
        let relayDir = root.appendingPathComponent("relay")
        let storeDir = root.appendingPathComponent("store")
        for dir in [relayDir, storeDir] {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        }
        defer { try? FileManager.default.removeItem(at: root) }

        let relay = try await RelayHandle(name: "c11e-relay", workDir: relayDir, binaryPath: binaryPath)
        try await relay.start()

        // The thing being commented on: an ordinary kind:1 note by someone
        // else, seeded over a real EVENT frame before the engine exists.
        let noteKeys = try NostrKeyPair()
        let rootNote = try NostrSigning.sign(
            keyPair: noteKeys, kind: 1, content: "C11: the note being commented on"
        )
        let seeded = try await relay.seed(rootNote)
        XCTAssertTrue(seeded, "PRECONDITION: the relay did not accept the root note")

        let engine = try NMPEngine(
            config: NMPConfig(
                storePath: storeDir.appendingPathComponent("nmp.redb").path,
                appRelays: [relay.url]
            )
        )
        defer { engine.shutdown() }
        _ = try engine.session.add(privateKey: NMPPrivateKey.generate(), makeCurrent: true)

        var log: [String] = []
        func note(_ line: String) { log.append(line) }
        defer { print((["", "C11 event-root phase log:"] + log).joined(separator: "\n")) }

        // Read the note back the ordinary way, so the comment is composed
        // off a row NMP delivered rather than off a hand-made value.
        let noteQuery = try engine.observe(
            .single(NMPDemand(selection: NMPFilter(kinds: [1], authors: .literal([noteKeys.pubkeyHex]))))
        )
        let noteLedger = RowLedger()
        let noteConsumer = Task {
            do {
                for try await batch in noteQuery { await noteLedger.record(batch) }
            } catch {}
        }
        defer {
            noteConsumer.cancel()
            noteQuery.cancel()
        }
        let gotNote = await waitUntil(timeout: 30) { await noteLedger.snapshot().has(rootNote.id) }
        let noteSnapshot = await noteLedger.snapshot()
        XCTAssertTrue(gotNote, "PRECONDITION: the seeded note never arrived -- \(noteSnapshot.report)")
        guard let noteRow = noteSnapshot.row(rootNote.id) else {
            return XCTFail("PRECONDITION: the note row vanished from the latest batch")
        }

        // The event-rooted thread this comment belongs to, and the demand
        // the capability offers for reading it. The demand's own selection
        // is printed as the captured evidence for #1876.
        let eventRoot = CommentRoot.event(
            eventID: rootNote.id, kind: 1, authorPubkey: noteKeys.pubkeyHex
        )
        let threadDemand = try commentThreadDemand(root: eventRoot)
        let demandTags = threadDemand.selection.tags
            .map { "\($0.key)=\($0.value)" }
            .sorted()
            .joined(separator: " ")
        note("event-root demand selection: kinds=\(threadDemand.selection.kinds ?? []) tags=\(demandTags)")
        note("root note id: \(rootNote.id)")

        let threadQuery = try engine.observe(.single(threadDemand))
        // The workaround an app is forced into today: the same read, with
        // NIP-22's uppercase-root tag shape hand-written by the app.
        let handBuiltQuery = try engine.observe(
            .single(NMPDemand(selection: NMPFilter(kinds: [1111], tags: ["E": .literal([rootNote.id])])))
        )
        let threadLedger = RowLedger()
        let handBuiltLedger = RowLedger()
        let consumers = Task {
            await withTaskGroup(of: Void.self) { group in
                group.addTask {
                    do {
                        for try await batch in threadQuery { await threadLedger.record(batch) }
                        await threadLedger.markEnded("sequence ended")
                    } catch { await threadLedger.markEnded("threw: \(error)") }
                }
                group.addTask {
                    do {
                        for try await batch in handBuiltQuery { await handBuiltLedger.record(batch) }
                        await handBuiltLedger.markEnded("sequence ended")
                    } catch { await handBuiltLedger.markEnded("threw: \(error)") }
                }
            }
        }
        defer { consumers.cancel() }

        var intent = try NMP.commentIntent(on: .row(noteRow), content: "C11: commenting on a note")
        intent.routing = .explicit(relays: [relay.url])
        let receipt = try await engine.publish(intent)

        let evidence = await awaitPublished(receipt, at: relay.url)
        note("comment publish: \(evidence.report)")
        XCTAssertTrue(
            evidence.publishedHere,
            "PRECONDITION: the event-rooted comment never reached RelayState.published -- \(evidence.report)"
        )
        guard let commentID = evidence.signedEventID else {
            return XCTFail("PRECONDITION: no signed event id -- \(evidence.report)")
        }
        let onRelay = try await relay.queryById(commentID)
        note("comment on relay by REQ: \(onRelay != nil) tags=\(onRelay?["tags"] ?? "nil")")
        XCTAssertNotNil(
            onRelay,
            "PRECONDITION: a real REQ for \(commentID) returned nothing -- the comment is not on the relay"
        )

        // The control: the comment IS readable, by an app that hand-builds
        // the tag scope itself. If this ever goes red, the finding below is
        // about something other than the demand.
        let handBuiltSaw = await waitUntil(timeout: 20) {
            await handBuiltLedger.snapshot().has(commentID)
        }
        let handBuiltFinal = await handBuiltLedger.snapshot()
        note("hand-built #E query: \(handBuiltFinal.report)")
        XCTAssertTrue(
            handBuiltSaw,
            "CONTROL: an ordinary NMPFilter(kinds: [1111], tags: [\"E\": ...]) did not deliver "
                + "\(commentID) either -- \(handBuiltFinal.report). The finding below is then not "
                + "specific to commentThreadDemand."
        )

        // The finding. 20s, bounded, because nothing is ever coming: the
        // wire filter asks for {"kinds":[1111],"#I":["<event id>"]} and the
        // comment carries no `I` tag at all.
        let threadSaw = await waitUntil(timeout: 20) {
            await threadLedger.snapshot().has(commentID)
        }
        let threadFinal = await threadLedger.snapshot()
        note("commentThreadDemand query: \(threadFinal.report)")
        XCTAssertTrue(
            threadSaw,
            "#1876, LEFT RED ON PURPOSE: commentThreadDemand(root: .event(...)) did not deliver "
                + "\(commentID) in 20s -- \(threadFinal.report) -- while the SAME comment, on the "
                + "SAME relay, in the SAME run, WAS delivered to a hand-built "
                + "NMPFilter(kinds: [1111], tags: [\"E\": ...]) (\(handBuiltFinal.report)). The "
                + "demand binds the root identifier to #I for every root shape (selection tags: "
                + "\(demandTags)), but an event root is written as an `E` tag. Two of the "
                + "capability's three root shapes cannot be read back through the capability's own "
                + "door. This assertion is written the right way round and goes green when #1876 "
                + "is fixed."
        )

        consumers.cancel()
        try await relay.kill()
    }

    /// `setup-strfry.sh`'s own default cache location. Same shape as every
    /// other scenario in this package: skip by name rather than crash.
    private static func locateStrfryBinary() throws -> URL {
        let cacheDir = ProcessInfo.processInfo.environment["RELAY_LAB_CACHE_DIR"]
            ?? (NSHomeDirectory() + "/Library/Caches/nmp-canary-relay-lab")
        let binary = URL(fileURLWithPath: cacheDir).appendingPathComponent("strfry/strfry")
        guard FileManager.default.isExecutableFile(atPath: binary.path) else {
            throw XCTSkip(
                "strfry is not built at \(binary.path) -- run apps/Canary/setup-strfry.sh first"
            )
        }
        return binary
    }

    private static func hex(_ data: Data) -> String {
        data.map { String(format: "%02x", $0) }.joined()
    }
}
