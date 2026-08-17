// C4 (docs/internals/canary.md "Scenario status", #1871): a reactive
// derived query -- a query built on top of another must update when the
// first one changes, with no app action at all.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C7/C9/C13/C16/C18.
// Public `NMP` API only: plain `import NMP`, no `@testable`, no internal
// crate, no direct Redb inspection. The relay is reached only over a real
// `ws://` URL to a separate process.
//
// THE COMPOSITION IS THE ONE AN ORDINARY APP WRITES ON DAY ONE. Not an
// abstract "derived binding" exercise: a feed of the people you follow.
// `NMPBinding.derived`'s own documentation gives exactly this example --
// "authors of my kind:3 contact list, projected through their `p` tags" --
// and nothing anywhere drove it end to end against a real relay through
// the public Swift API. Every scenario written so far uses
// `.literal(...)`, i.e. an author set the app already knew.
//
// This runs IN PROCESS, unlike C16/C17/C18 and their children. That is
// deliberate rather than inconsistent: C4 measures no process-wide
// quantity (#1796 does not apply), needs no crash and no cold read, and
// its whole claim is that ONE never-reopened query handle changes what it
// delivers while the process keeps running -- which a restart would
// destroy rather than prove.
//
// THREE THINGS HAVE TO BE TRUE FOR THIS NOT TO BE VACUOUS, and each is
// asserted with the real captured values:
//
//   1. the derived query really is derived -- before anything changes, it
//      delivers the FOLLOWED author's note and does NOT deliver the
//      unfollowed author's note, which is on the same relay, matches the
//      same kind, and is proven deliverable by a control observation over
//      a literal filter naming that author. Without the negative, a query
//      matching everything would pass the reactive half perfectly;
//   2. the BASE query really changed -- the kind:3 contact list is watched
//      through its own separate handle, and its projected `p` set must be
//      seen to grow from one author to two. A derived query that was
//      already correct proves nothing about reactivity, and this is the
//      C13-fourth-falsifier lesson applied to a semantic change instead of
//      an outage;
//   3. nothing on the app side is touched across the change. No reopened
//      query, no second engine, no re-`observe`. The one handle opened at
//      the top of the scenario is the one asserted on at the bottom.
//
// Every wait is a bounded poll on a real condition with the real stuck
// value reported on timeout -- never a fixed sleep used AS the oracle.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C4ReactiveDerivedQueryTests: XCTestCase {
    /// What one observation has been delivered. `latest` is the newest
    /// batch's row set (an `NMPQuery` element is the full current snapshot)
    /// and `everSeen` is the union across every batch, because "it started
    /// delivering X" and "it never delivered X" are different questions:
    /// the negative in phase 1 must be asserted against everything the
    /// query has EVER shown, or a row delivered once and dropped would slip
    /// through it.
    private actor Observed {
        private(set) var latest: [Row] = []
        private(set) var everSeen = Set<String>()
        private(set) var batches = 0
        private(set) var ended: String?

        func record(_ batch: RowBatch) {
            batches += 1
            latest = batch.rows
            everSeen.formUnion(batch.rows.map(\.id))
        }

        func markEnded(_ why: String) { ended = why }

        var latestIDs: Set<String> { Set(latest.map(\.id)) }

        /// The `p` tag values across every delivered row -- for the contact
        /// list observation, exactly the set the derived binding projects.
        var projectedPTags: Set<String> {
            Set(latest.flatMap { $0.tags }.filter { $0.first == "p" && $0.count >= 2 }.map { $0[1] })
        }
    }

    func testAFeedDerivedFromAContactListUpdatesWhenTheContactListChanges() async throws {
        let binaryPath = try Self.locateStrfryBinary()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c4-\(UUID().uuidString)")
        let relayDir = root.appendingPathComponent("relay")
        let storeDir = root.appendingPathComponent("store")
        try FileManager.default.createDirectory(at: relayDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: storeDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let relay = try await RelayHandle(name: "c4-relay", workDir: relayDir, binaryPath: binaryPath)
        try await relay.start()
        defer { Task { try? await relay.kill() } }

        // Three real identities: the reader, and two people they might
        // follow. `alice` is followed from the start; `bob` is not, and
        // becomes followed halfway through.
        let me = try NostrKeyPair()
        let alice = try NostrKeyPair()
        let bob = try NostrKeyPair()

        var log: [String] = []
        func note(_ line: String) { log.append(line) }
        defer { print((["", "C4 phase log:"] + log).joined(separator: "\n")) }

        // Everything below is seeded over real EVENT frames with real OKs.
        // Both notes exist on the relay from the start, so the only thing
        // that can ever change which of them the feed delivers is the
        // contact list.
        let followAliceOnly = try NostrSigning.sign(
            keyPair: me, kind: 3, tags: [["p", alice.pubkeyHex]], content: "",
            createdAt: Int64(Date().timeIntervalSince1970) - 60
        )
        let aliceNote = try NostrSigning.sign(keyPair: alice, kind: 1, content: "C4 alice note")
        let bobNote = try NostrSigning.sign(keyPair: bob, kind: 1, content: "C4 bob note")
        try await relay.seed(followAliceOnly)
        try await relay.seed(aliceNote)
        try await relay.seed(bobNote)

        let engine = try NMPEngine(
            config: NMPConfig(
                storePath: storeDir.appendingPathComponent("nmp.redb").path,
                appRelays: [relay.url]
            )
        )
        defer { engine.shutdown() }

        // THE COMPOSITION UNDER TEST, written exactly as an app would.
        //
        // The inner demand is a complete live query in its own right: my
        // kind:3 contact list. The outer filter's `authors` is that query's
        // rows projected through their `p` tags. `.pinned` on both because
        // the lab has one relay and no NIP-65 indexers -- `.authorOutboxes`
        // would need `NMPConfig.outboxRouting`, which is the Canary's
        // already-recorded read/write asymmetry finding and not C4's
        // subject.
        let contactListSelection = NMPFilter(kinds: [3], authors: .literal([me.pubkeyHex]))
        let follows = NMPBinding.derived(
            inner: NMPDemand(selection: contactListSelection, source: .pinned([relay.url])),
            project: .tag("p")
        )
        let feed = try engine.observe(
            NMPDemand(
                selection: NMPFilter(kinds: [1], authors: follows),
                source: .pinned([relay.url])
            )
        )

        // The BASE query, through its own separate handle. This exists only
        // to make claim 2 above assertable: it is never used to drive the
        // feed, and closing it would change nothing about the feed.
        let contactList = try engine.observe(
            NMPDemand(selection: contactListSelection, source: .pinned([relay.url]))
        )

        // The CONTROL: a literal-author query for bob. Its job is to prove
        // bob's note is genuinely deliverable from this relay, so that
        // "the feed does not show bob" is a statement about the derived
        // binding and not about a note that never arrived at all.
        let bobControl = try engine.observe(
            NMPDemand(
                selection: NMPFilter(kinds: [1], authors: .literal([bob.pubkeyHex])),
                source: .pinned([relay.url])
            )
        )

        let feedState = Observed()
        let contactListState = Observed()
        let controlState = Observed()
        let consumers = Task {
            await withTaskGroup(of: Void.self) { group in
                group.addTask {
                    do {
                        for try await batch in feed { await feedState.record(batch) }
                        await feedState.markEnded("sequence ended")
                    } catch { await feedState.markEnded("threw: \(error)") }
                }
                group.addTask {
                    do {
                        for try await batch in contactList { await contactListState.record(batch) }
                        await contactListState.markEnded("sequence ended")
                    } catch { await contactListState.markEnded("threw: \(error)") }
                }
                group.addTask {
                    do {
                        for try await batch in bobControl { await controlState.record(batch) }
                        await controlState.markEnded("sequence ended")
                    } catch { await controlState.markEnded("threw: \(error)") }
                }
            }
        }
        defer { consumers.cancel() }

        // --- Phase 1: the derived feed resolves, and resolves NARROWLY ----

        let feedSawAlice = await waitUntil(timeout: 30) {
            await feedState.latestIDs.contains(aliceNote.id)
        }
        let controlSawBob = await waitUntil(timeout: 30) {
            await controlState.latestIDs.contains(bobNote.id)
        }
        // Only once the control has bob's note is "the feed does not have
        // bob's note" a fact about the derived author set rather than about
        // timing. The control and the feed share one engine and one relay,
        // so the note has demonstrably arrived and been stored by then.
        let feedEverSawBob = await feedState.everSeen.contains(bobNote.id)
        let baseBefore = await contactListState.projectedPTags
        let feedBefore = await feedState.latestIDs
        // Every value an assertion message needs is captured here, before
        // the assertions: an `await` inside an XCTAssert message is an
        // autoclosure that cannot suspend.
        let feedBatchesBefore = await feedState.batches
        let feedEndedBefore = await feedState.ended ?? "no"
        let controlRows = await controlState.latestIDs.count
        note(
            "before: feedSawAlice=\(feedSawAlice) controlSawBob=\(controlSawBob) "
                + "feedEverSawBob=\(feedEverSawBob) feed=\(feedBefore.count) rows "
                + "(\(feedBatchesBefore) batches) contactList p-tags=\(baseBefore.count) "
                + "control=\(controlRows) rows"
        )
        XCTAssertTrue(
            feedSawAlice,
            "PRECONDITION: the derived feed never delivered the followed author's note within "
                + "30s. It holds \(feedBefore) after \(feedBatchesBefore) batches "
                + "(ended \(feedEndedBefore)). The composition an app would write "
                + "does not resolve at all, which is an NMP API finding rather than a timing one."
        )
        XCTAssertTrue(
            controlSawBob,
            "PRECONDITION: the control query over a LITERAL author never delivered bob's note, "
                + "so this relay never served it and the negative assertion below would be "
                + "about a missing event rather than about the derived author set."
        )
        XCTAssertEqual(
            baseBefore, [alice.pubkeyHex],
            "PRECONDITION: the contact list observation projects \(baseBefore.count) `p` tag(s) "
                + "before the change, expected exactly alice. The derived binding resolves the "
                + "same projection, so if this is already wrong the phases below mean nothing."
        )
        // THE NEGATIVE. A derived query that quietly resolved to "everyone"
        // would satisfy every positive assertion in this file.
        XCTAssertFalse(
            feedEverSawBob,
            "the derived feed delivered the UNFOLLOWED author's note (\(bobNote.id)) before the "
                + "contact list ever named him. The binding is not narrowing to the projected "
                + "author set, so nothing below would be evidence of reacting to a change."
        )
        XCTAssertEqual(
            feedBefore, [aliceNote.id],
            "the derived feed holds \(feedBefore) before the change, expected exactly the one "
                + "followed author's note"
        )

        // --- Phase 2: the BASE query genuinely changes ---------------------
        //
        // A NEW kind:3 with a later `created_at`, which is a replacement
        // rather than an addition (kind 3 is replaceable), published over a
        // real EVENT frame with a real OK. Nothing in the app is touched.

        let followAliceAndBob = try NostrSigning.sign(
            keyPair: me, kind: 3,
            tags: [["p", alice.pubkeyHex], ["p", bob.pubkeyHex]], content: "",
            createdAt: Int64(Date().timeIntervalSince1970)
        )
        // Caught rather than propagated so the refusal REASON reaches the
        // log. strfry rejects a superseded replaceable event outright
        // ("replaced: have newer event"), and a bare `try` turns that into
        // an unlabelled thrown error at the wrong file and line.
        var seeded = false
        var seedRefusal = "none"
        do { seeded = try await relay.seed(followAliceAndBob) }
        catch { seedRefusal = "\(error)" }

        let baseChanged = await waitUntil(timeout: 45) {
            await contactListState.projectedPTags == [alice.pubkeyHex, bob.pubkeyHex]
        }
        let baseAfter = await contactListState.projectedPTags
        let baseBatches = await contactListState.batches
        let baseRows = await contactListState.latest.count
        note(
            "base change: seeded=\(seeded) (\(seedRefusal)) baseChanged=\(baseChanged) "
                + "contactList p-tags \(baseBefore.count) -> \(baseAfter.count) "
                + "(\(baseBatches) batches, rows \(baseRows))"
        )
        XCTAssertTrue(
            seeded,
            "the relay did not accept the replacement contact list: \(seedRefusal)"
        )
        XCTAssertTrue(
            baseChanged,
            "PRECONDITION: the BASE query did not change. 45s after publishing a replacement "
                + "kind:3 naming both authors, the contact list observation still projects "
                + "\(baseAfter). Everything the reactive assertion below claims would be vacuous: "
                + "a derived query cannot be shown to follow a change that never happened."
        )

        // --- Phase 3: the derived feed follows, with NO app action --------
        //
        // The feed handle opened in phase 1 has not been touched: not
        // cancelled, not reopened, not re-`observe`d, and the engine is the
        // same one. If NMP resolves a derived binding once at subscribe
        // time, this is where it fails.

        let feedFollowed = await waitUntil(timeout: 60) {
            await feedState.latestIDs.contains(bobNote.id)
        }
        let feedAfter = await feedState.latestIDs
        let feedRows = await feedState.latest
        let feedBatchesAfter = await feedState.batches
        let feedEndedAfter = await feedState.ended ?? "no"
        note(
            "reactive: feedFollowed=\(feedFollowed) feed \(feedBefore.count) -> "
                + "\(feedAfter.count) rows over \(feedBatchesAfter) batches "
                + "(ended \(feedEndedAfter))"
        )
        XCTAssertTrue(
            feedFollowed,
            "THE CLAIM: 60s after the contact list provably grew to name both authors, the "
                + "derived feed still holds \(feedAfter). It did not react to its own base query "
                + "changing, so an app has to tear down and reopen its feed on every follow -- "
                + "the app-side lifecycle hack the Canary exists to surface."
        )
        XCTAssertEqual(
            feedAfter, [aliceNote.id, bobNote.id],
            "the derived feed holds \(feedAfter) after the change, expected exactly both authors' "
                + "notes. Missing one is a lost row; an extra one means the binding widened past "
                + "the projected set."
        )
        XCTAssertEqual(
            feedRows.count, 2,
            "the derived feed delivered \(feedRows.count) rows against 2 expected ids. Above the "
                + "id count this is a duplicate canonical row; below it, a lost event."
        )
        // The previously-delivered row survived the change. A derived query
        // that reacts by REPLACING its result set rather than re-resolving
        // it would drop alice's note here and still satisfy the assertion
        // above's `contains`.
        XCTAssertTrue(
            feedAfter.contains(aliceNote.id),
            "the already-followed author's note vanished from the feed when the contact list "
                + "grew -- reacting to the base query must re-resolve the author set, never "
                + "restart the result set"
        )

        consumers.cancel()
    }

    @discardableResult
    private func waitUntil(
        timeout: TimeInterval,
        _ condition: () async -> Bool
    ) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if await condition() { return true }
            try? await Task.sleep(nanoseconds: 25_000_000)
        }
        return await condition()
    }

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
}
