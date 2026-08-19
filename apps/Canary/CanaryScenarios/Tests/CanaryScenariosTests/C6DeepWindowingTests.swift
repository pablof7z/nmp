// C6 (docs/internals/canary.md "Scenario status"): deep windowing. An app
// scrolls far back through a real relay's history -- many pages, not one --
// and must get correct order, no gaps, no duplicates, and no unbounded
// growth in what it is holding.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C2/C7/C9/C13/C17.
// Public `NMP` API only: plain `import NMP`, no `@testable`, no internal
// crate, no direct Redb inspection. The relay is reached only over a real
// `ws://` URL to a separate strfry process.
//
// WHAT MAKES THIS DEEP RATHER THAN A PAGINATION SMOKE TEST. One
// `requestRows` proves the call exists. The failures that make an infinite
// scroll unusable only appear across MANY advances against a history bigger
// than the window will ever be:
//
//   - ORDER. Every page must be the canonical newest-first prefix
//     (`createdAt DESC, id ASC`, `Window.expandable`'s own contract),
//     compared against an expected order this scenario computes from the
//     events it signed -- never against whatever NMP happened to deliver.
//   - NO GAPS. Page N must be exactly the top N, so an event skipped in the
//     middle fails at the page that should have contained it, naming it.
//   - NO DUPLICATES, and NO RESHUFFLING. Each page must extend the previous
//     one: `Array(page.prefix(previous.count)) == previous`. A row that
//     moves, repeats, or vanishes as the window grows fails here even
//     though the final set would be right.
//   - NO UNBOUNDED GROWTH. The app must never be holding more than it
//     asked for. Every delivered batch is checked against the target in
//     force when it arrived, `max` is proven to actually bound (a request
//     past it lands as `.atBound(max:)`, a delivered fact, not a throw),
//     and a live event arriving at a full window must displace the oldest
//     row rather than making the window one bigger.
//
// THE PRECONDITION IS ASSERTED, NOT ASSUMED. A windowed observation whose
// window never advanced past its first page would satisfy "correct order,
// no duplicates" perfectly and prove nothing at all. So before any of the
// deep assertions, this scenario requires with the real captured numbers:
//
//   1. the window really was BOUND at the start -- the first page carries
//      exactly `initial` rows against a relay holding many times that, so
//      there is somewhere to scroll FROM;
//   2. the opening request really was ANSWERED -- the relay reported
//      `finishedStoredEvents` to THIS observation, so the app is in the
//      settled first-page state a user scrolls from rather than racing the
//      opening load. That status is deliberately NOT read as "all the
//      history is local": its own doc calls it "a delivery fact about ONE
//      source answering ONE request", and a windowed observation's opening
//      request carries the window's `initial` as its wire limit;
//   3. the window really ADVANCED -- a later page carries strictly more
//      rows than an earlier one AND ids the app was not already holding.
//
// Note what precondition 3 does NOT use. `WindowLoad.returned(added:)`
// looks like the natural in-band progress signal and is not usable as one:
// measured across repeated runs, the same advance reported
// `.returned(added: 20)` on some runs and `.returned(added: 0)` on others,
// with the rows arriving in a LATER batch carrying `.idle`. That is
// recorded as an observation at the site rather than asserted in either
// direction.
//
// THIS SCENARIO IS RED ON PURPOSE. It found a real defect -- the FIRST
// window advance is dropped -- and the assertion for it is left failing
// rather than relaxed. See the phase named for it below.
//
// Every wait below is a bounded poll on a real condition with the real
// stuck values reported on timeout -- never a fixed sleep used AS the
// synchronization oracle.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C6DeepWindowingTests: XCTestCase {
    /// How much history the relay holds. Deliberately larger than the
    /// window's `max`, so the deepest page is still a window over a bigger
    /// history rather than "everything there is".
    private static let historyCount = 150
    /// The window's opening size, its ceiling, and the step each advance
    /// raises the target by.
    private static let initialRows: UInt64 = 10
    private static let maxRows: UInt64 = 100
    private static let pageStep: UInt64 = 10

    // MARK: - What the windowed observation has been delivered

    private struct ObservedState: Sendable {
        var batches = 0
        var latest: [Row] = []
        var latestLoad: WindowLoad?
        /// Every `WindowLoad` delivered, in arrival order.
        var loadHistory: [WindowLoad] = []
        /// Non-nil iff some batch carried the same event id twice.
        var duplicateWitness: String?
        /// Non-nil iff some batch carried more rows than the target that was
        /// in force when it arrived -- the unbounded-growth failure, caught
        /// at the batch that caused it rather than at the end.
        var overGrowthWitness: String?
        /// The largest row count any batch ever carried.
        var maxRowCount = 0
        /// Relays that have, at some point, told THIS observation they sent
        /// everything they hold for it (`finishedStoredEvents`) or that its
        /// coverage is satisfied. The only public fact that says "the
        /// history is in", and the precondition the deep scroll needs: "no
        /// gaps" is not a claim you can make against a store still filling.
        var acquiredRelays: Set<String> = []
        /// Latest per-relay status label, printed as evidence.
        var relayStatus: [String: String] = [:]
        /// Every distinct status label reported for the lab relay, in
        /// arrival order. Printed on every run: it is what shows that the
        /// dropped advance below coincides with the source failing.
        var statusHistory: [String] = []
        var ended: String?
    }

    private actor ObservationLedger {
        private var state = ObservedState()
        /// The window target currently in force. Raised by the driver BEFORE
        /// it calls `requestRows`, so a batch carrying more rows than this
        /// was never asked for by anyone.
        private var ceiling: Int

        init(ceiling: Int) { self.ceiling = ceiling }

        func raiseCeiling(to newValue: Int) { ceiling = max(ceiling, newValue) }

        func record(_ batch: RowBatch) {
            state.batches += 1
            let ids = batch.rows.map(\.id)
            if Set(ids).count != ids.count {
                state.duplicateWitness = ids.joined(separator: " ")
            }
            if ids.count > ceiling {
                state.overGrowthWitness =
                    "batch \(state.batches) carried \(ids.count) rows against a window target of "
                    + "\(ceiling)"
            }
            state.latest = batch.rows
            state.maxRowCount = max(state.maxRowCount, ids.count)
            if let load = batch.load {
                state.latestLoad = load
                state.loadHistory.append(load)
            }
            for source in batch.evidence.first?.sources ?? [] {
                let label = Self.label(source.status)
                state.relayStatus[source.relay] = label
                if state.statusHistory.last != label { state.statusHistory.append(label) }
                if label == "finishedStoredEvents" || label == "coverageSatisfied" {
                    state.acquiredRelays.insert(source.relay)
                }
            }
        }

        /// A short label per `SourceStatus` case, spelled out rather than
        /// `String(describing:)` so the "the history is in" classification is
        /// a decision this scenario makes in the open.
        static func label(_ status: SourceStatus) -> String {
            switch status {
            case .requesting: return "requesting"
            case .finishedStoredEvents: return "finishedStoredEvents"
            case .awaitingRequest: return "awaitingRequest"
            case .coverageSatisfied: return "coverageSatisfied"
            case .connecting: return "connecting"
            case .disconnected: return "disconnected"
            case .awaitingAuth: return "awaitingAuth"
            case .authDenied: return "authDenied"
            case .error: return "error"
            }
        }

        func markEnded(_ why: String) { state.ended = why }
        func current() -> ObservedState { state }
    }

    /// Bounded poll on a real condition. Returns whether the condition ever
    /// held; the caller reports the real stuck values on `false`. The sleep
    /// paces the poll -- it is never the thing being waited on.
    @discardableResult
    private func waitUntil(
        timeout: TimeInterval = 45,
        _ condition: () async -> Bool
    ) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if await condition() { return true }
            try? await Task.sleep(nanoseconds: 25_000_000)
        }
        return await condition()
    }

    // MARK: - The scenario

    func testScrollingFarBackThroughHistoryStaysOrderedGaplessAndBounded() async throws {
        let binaryPath = try Self.locateStrfryBinary()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c6-\(UUID().uuidString)")
        let relayDir = root.appendingPathComponent("relay")
        let storeDir = root.appendingPathComponent("store")
        try FileManager.default.createDirectory(at: relayDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: storeDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let relay = try await RelayHandle(name: "c6-relay", workDir: relayDir, binaryPath: binaryPath)
        try await relay.start()

        let keyPair = try NostrKeyPair()
        let filter = NMPFilter(kinds: [1], authors: .literal([keyPair.pubkeyHex]))

        // A real history: `historyCount` distinct events, one second apart,
        // all seeded over real EVENT frames before the engine exists. Every
        // `created_at` is distinct, so the canonical `createdAt DESC, id ASC`
        // order is fully determined and this scenario can compute the
        // expected order itself instead of accepting whatever arrives.
        let newest = Int64(Date().timeIntervalSince1970) - 10
        var history: [NostrEvent] = []
        let seedStarted = Date()
        for index in 0..<Self.historyCount {
            let event = try NostrSigning.sign(
                keyPair: keyPair, kind: 1,
                content: "C6 history #\(index)",
                createdAt: newest - Int64(index)
            )
            let accepted = try await relay.seed(event)
            XCTAssertTrue(accepted, "the relay refused history event #\(index)")
            history.append(event)
        }
        let seedSeconds = Date().timeIntervalSince(seedStarted)
        // Newest first. `history` is already built newest-first, but sorting
        // by the documented canonical key rather than trusting construction
        // order keeps the expectation honest if the seeding loop ever changes.
        let expectedOrder = history.sorted {
            $0.created_at == $1.created_at ? $0.id < $1.id : $0.created_at > $1.created_at
        }.map(\.id)

        let engine = try NMPEngine(
            config: NMPConfig(
                storePath: storeDir.appendingPathComponent("nmp.redb").path,
                appRelays: [relay.url]
            )
        )
        defer { engine.shutdown() }

        // The windowed observation. ONE handle, open for the whole scroll:
        // reopening it with a bigger window would prove the engine can serve
        // a big first page, not that an app can scroll.
        let query = try engine.observe(
            .single(NMPDemand(selection: filter)), window: .expandable(initial: Self.initialRows, max: Self.maxRows)
        )
        let ledger = ObservationLedger(ceiling: Int(Self.initialRows))
        let consumer = Task {
            do {
                for try await batch in query { await ledger.record(batch) }
                await ledger.markEnded("sequence ended")
            } catch {
                await ledger.markEnded("threw: \(error)")
            }
        }
        defer { consumer.cancel() }

        var log: [String] = []
        func note(_ line: String) { log.append(line) }
        defer { print((["", "C6 phase log:"] + log).joined(separator: "\n")) }
        note(
            "seeded \(Self.historyCount) events in \(String(format: "%.1f", seedSeconds))s; "
                + "window initial=\(Self.initialRows) max=\(Self.maxRows) step=\(Self.pageStep)"
        )

        // --- Precondition 1: the window is genuinely BOUND -----------------
        //
        // The relay holds `historyCount` matching events. If the first page
        // arrives with all of them, there is no window, and every "no
        // unbounded growth" claim below would be about a set that was never
        // bounded in the first place.

        let firstPageArrived = await waitUntil(timeout: 45) {
            await ledger.current().latest.count == Int(Self.initialRows)
        }
        // AND the relay has genuinely finished answering the opening
        // request, so the first page is a settled state rather than a
        // partially-filled one. Note precisely what this does and does not
        // establish: `SourceStatus.finishedStoredEvents`'s own doc says it
        // is "a delivery fact about ONE source answering ONE request --
        // never a claim that the query is complete", and a windowed
        // observation's opening request carries the window's `initial` as
        // its wire limit, so this emphatically does NOT mean the whole
        // history is local. It means the app is in a stable first-page
        // state, which is the state a user scrolls from. Every page below
        // therefore waits on the delivered ROW COUNT and checks the page
        // against a canonical order this scenario computed itself, rather
        // than trusting any status to mean "everything is here".
        let historyAcquired = await waitUntil(timeout: 45) {
            await ledger.current().acquiredRelays.contains(relay.url)
        }
        // A settle beat: if the window were going to keep growing on its own
        // past `initial`, this is where it would show, and the assertion
        // below would catch it rather than a lucky sample.
        try? await Task.sleep(nanoseconds: 500_000_000)
        let firstPage = await ledger.current()
        let firstIDs = firstPage.latest.map(\.id)
        note(
            "page \(Self.initialRows): arrived=\(firstPageArrived) rows=\(firstPage.latest.count) "
                + "historyAcquired=\(historyAcquired) status=\(firstPage.relayStatus) "
                + "batches=\(firstPage.batches) load=\(String(describing: firstPage.latestLoad)) "
                + "loadHistory=\(firstPage.loadHistory)"
        )
        XCTAssertTrue(
            historyAcquired,
            "PRECONDITION: \(relay.url) never reported `finishedStoredEvents`/`coverageSatisfied` "
                + "to this observation (statuses \(firstPage.relayStatus)). Until the opening "
                + "request has been answered, the app is not in the settled first-page state a "
                + "user would be scrolling from, and the advance below would be racing the "
                + "opening load rather than testing a scroll."
        )
        XCTAssertTrue(
            firstPageArrived && firstIDs.count == Int(Self.initialRows),
            "PRECONDITION: the opening page must carry exactly \(Self.initialRows) rows against a "
                + "relay holding \(Self.historyCount) matching events -- got \(firstIDs.count) "
                + "across \(firstPage.batches) batch(es), load "
                + "\(String(describing: firstPage.latestLoad)), ended \(firstPage.ended ?? "no"). "
                + "An unbounded first page means there is no window here to scroll or to bound."
        )
        assertPage(
            firstIDs, expectedPrefixOf: expectedOrder, target: Int(Self.initialRows),
            previous: [], step: "page \(Self.initialRows)"
        )

        // --- The FIRST advance: was a real defect, now FIXED in Rust ------
        //
        // #1886, and the assertion below has always asserted the CORRECT
        // behaviour -- it was left red rather than relaxed. The engine-side
        // cause is fixed: a staged advance attached its wire handles without
        // arming wire admission (retaining an atom only marks it pending;
        // admission is what compiles it into a REQ), and the runtime
        // discarded the staged turn's effects on the success path while
        // dispatching them on failure. Falsified in Rust by
        // `crates/nmp/tests/expandable_window_first_advance.rs`, which drives
        // the production runtime against a relay that reports its own wire
        // record. This scenario has NOT been re-run since that fix, so treat
        // it as unverified rather than known-green.
        //
        // What was measured, for the record. `requestRows(atLeast:)` is
        // documented as monotonically raising the window's row target, with
        // outcomes delivered in band. The FIRST call that actually raised a
        // window's target raised nothing: it delivered `.returned(added: 0)`
        // and the window stayed at `initial` rows indefinitely -- measured at
        // 10 rows against a target of 20 after a bounded 45s wait, with the
        // relay up and holding all 150 matching events the whole time. It
        // never self-healed. Re-issuing the SAME target could not fix it
        // either, because `requestRows` is documented as a no-op at
        // or below the current target, so an app had no way to ask again
        // from where it was. Only raising the target AGAIN moved the window,
        // and that next raise then delivered its own value exactly.
        //
        // Reproduced 5/5 across (initial, firstTarget) of (10,11), (10,20),
        // (10,50), (1,2) and (10,10)->(10,11), with 0ms/1s/3s settle beats
        // before the call, so it is neither a race nor a function of the
        // step size.
        //
        // What the public API shows about the mechanism, printed on every
        // run: the query's OWN `SourceEvidence.status` for this relay walks
        // `finishedStoredEvents` -> `error` -> `awaitingRequest` ->
        // `requesting` across this one advance. The relay's own log shows a
        // NIP-77 negentropy session opened, matching all the events, and
        // closed in exactly that window. The real mechanism was simpler than
        // that trace suggested: the second advance's commit supersedes the
        // first advance's handles, and the withdrawal that follows arms
        // admission as a side effect -- which is why the NEXT advance always
        // worked.
        //
        // In an app this was the first scroll-to-bottom doing nothing. The
        // assertion was NOT relaxed to match the behaviour and the scenario
        // was NOT reordered to warm the window up first: per
        // docs/internals/canary.md, awkward Canary code is a product bug
        // until shown otherwise, and C17's `distinct` phase is the standing
        // precedent for leaving a measured failure red rather than tuning
        // the oracle until it passes.

        let firstAdvanceTarget = Int(Self.initialRows + Self.pageStep)
        await ledger.raiseCeiling(to: firstAdvanceTarget)
        let loadsBeforeFirstAdvance = await ledger.current().loadHistory.count
        try query.requestRows(atLeast: UInt64(firstAdvanceTarget))
        let firstAdvanceLanded = await waitUntil(timeout: 45) {
            await ledger.current().latest.count == firstAdvanceTarget
        }
        let afterFirstAdvance = await ledger.current()
        let firstAdvanceLoads = Array(
            afterFirstAdvance.loadHistory.dropFirst(loadsBeforeFirstAdvance)
        )
        note(
            "first advance to \(firstAdvanceTarget) (#1886): landed=\(firstAdvanceLanded) "
                + "rows=\(afterFirstAdvance.latest.count) loads=\(firstAdvanceLoads) "
                + "status=\(afterFirstAdvance.relayStatus) "
                + "statusHistory=\(afterFirstAdvance.statusHistory)"
        )
        XCTAssertTrue(
            firstAdvanceLanded && afterFirstAdvance.latest.count == firstAdvanceTarget,
            "#1886 -- THE FIRST WINDOW ADVANCE IS DROPPED. `requestRows(atLeast: "
                + "\(firstAdvanceTarget))` left the window holding \(afterFirstAdvance.latest.count) "
                + "rows after a bounded 45s wait, delivering \(firstAdvanceLoads). \(relay.url) is "
                + "up and holds all \(Self.historyCount) matching events, and every later advance "
                + "in this same run reaches its target exactly, so the rows are obtainable and the "
                + "window simply did not get them. Re-issuing the same target is a documented "
                + "no-op, so an app in this state cannot ask again -- the first scroll-to-bottom "
                + "does nothing and no retry at the same position fixes it. The source's own "
                + "status walked \(afterFirstAdvance.statusHistory) across this advance."
        )

        // --- Precondition: the window genuinely DOES advance --------------
        //
        // The deep scroll below is only a claim about scrolling if the
        // window really moves, so that is asserted here on its own terms --
        // separately from #1886 above, and against the state the app is
        // ACTUALLY in rather than the state it should have been in.

        let secondTarget = firstAdvanceTarget + Int(Self.pageStep)
        let heldBeforeAdvance = afterFirstAdvance.latest.map(\.id)
        await ledger.raiseCeiling(to: secondTarget)
        let loadsBeforeAdvance = await ledger.current().loadHistory.count
        try query.requestRows(atLeast: UInt64(secondTarget))
        let advanced = await waitUntil(timeout: 45) {
            await ledger.current().latest.count == secondTarget
        }
        let secondPage = await ledger.current()
        let secondIDs = secondPage.latest.map(\.id)
        let newIDs = Set(secondIDs).subtracting(heldBeforeAdvance)
        let addedFacts = secondPage.loadHistory.dropFirst(loadsBeforeAdvance).compactMap {
            if case .returned(let added) = $0, added > 0 { return added } else { return nil }
        }
        note(
            "page \(secondTarget): advanced=\(advanced) rows=\(secondIDs.count) "
                + "newIDs=\(newIDs.count) addedFacts=\(addedFacts) "
                + "loadsSinceRequest=\(Array(secondPage.loadHistory.dropFirst(loadsBeforeAdvance))) "
                + "status=\(secondPage.relayStatus) load=\(String(describing: secondPage.latestLoad))"
        )
        XCTAssertTrue(
            advanced && secondIDs.count == secondTarget,
            "PRECONDITION: `requestRows(atLeast: \(secondTarget))` left the window holding "
                + "\(secondIDs.count) rows after 45s (loads \(secondPage.loadHistory), ended "
                + "\(secondPage.ended ?? "no")). The window never advanced past its first page, so "
                + "nothing below is a claim about scrolling."
        )
        XCTAssertEqual(
            newIDs.count, secondTarget - heldBeforeAdvance.count,
            "PRECONDITION: the advance must reach \(secondTarget - heldBeforeAdvance.count) ids the "
                + "app was not already holding; it reached \(newIDs.count). A page that grows "
                + "without reaching further back is not scrolling through history."
        )
        // NOT an assertion -- a recorded observation, and the second half of
        // the finding above. `WindowLoad.returned(added:)` does NOT reliably
        // carry the number of rows an advance added: across repeated runs
        // this same advance reported `.returned(added: 20)` sometimes and
        // `.returned(added: 0)` other times, with the 20 rows arriving in a
        // LATER batch carrying `.idle`. So an app cannot use the load fact
        // to decide whether its scroll produced anything -- the only
        // reliable signal is the delivered row count, which is what the two
        // assertions above use. `addedFacts` is printed on every run so
        // neither value is silently promoted to a contract (the same
        // treatment C13 gave `wireSubCount` during an outage).
        assertPage(
            secondIDs, expectedPrefixOf: expectedOrder, target: secondTarget,
            previous: heldBeforeAdvance, step: "page \(secondTarget)"
        )

        // --- The deep scroll ----------------------------------------------
        //
        // Every remaining page to the window's ceiling, each one checked
        // against the canonical expectation AND against its predecessor.

        var previous = secondIDs
        var target = secondTarget
        while target < Int(Self.maxRows) {
            target = min(target + Int(Self.pageStep), Int(Self.maxRows))
            await ledger.raiseCeiling(to: target)
            try query.requestRows(atLeast: UInt64(target))
            let reached = await waitUntil(timeout: 45) {
                await ledger.current().latest.count == target
            }
            let page = await ledger.current()
            let ids = page.latest.map(\.id)
            note(
                "page \(target): reached=\(reached) rows=\(ids.count) batches=\(page.batches) "
                    + "load=\(String(describing: page.latestLoad)) status=\(page.relayStatus) "
                    + "loads=\(page.loadHistory.suffix(6))"
            )
            XCTAssertTrue(
                reached && ids.count == target,
                "the window stalled at \(ids.count) rows when \(target) were requested "
                    + "(load \(String(describing: page.latestLoad)), \(page.batches) batches, "
                    + "ended \(page.ended ?? "no")). \(Self.historyCount) matching events exist on "
                    + "the relay, so there is more history to reach."
            )
            assertPage(
                ids, expectedPrefixOf: expectedOrder, target: target,
                previous: previous, step: "page \(target)"
            )
            previous = ids
        }

        // --- The ceiling really is a ceiling -------------------------------
        //
        // `max` is a bound, and a request past it is a delivered
        // `.atBound(max:)` FACT rather than a thrown error (`WindowLoad`'s
        // own contract). Both halves are asserted: the fact arrives, and the
        // row count does not move.

        let beyond = UInt64(Self.historyCount) + 50
        let loadsBeforeBound = await ledger.current().loadHistory.count
        try query.requestRows(atLeast: beyond)
        let reportedAtBound = await waitUntil(timeout: 30) {
            await ledger.current().loadHistory.dropFirst(loadsBeforeBound).contains {
                $0 == .atBound(max: Self.maxRows)
            }
        }
        // A settle beat: an over-the-bound advance that is going to leak rows
        // needs time to do it, and this is the one place row count could grow
        // without any further request from the app.
        try? await Task.sleep(nanoseconds: 1_000_000_000)
        let bounded = await ledger.current()
        note(
            "at bound: requested=\(beyond) reported=\(reportedAtBound) "
                + "rows=\(bounded.latest.count) maxRowsEverHeld=\(bounded.maxRowCount) "
                + "load=\(String(describing: bounded.latestLoad))"
        )
        XCTAssertTrue(
            reportedAtBound,
            "requesting \(beyond) rows on a window declared `max: \(Self.maxRows)` never delivered "
                + "`.atBound(max: \(Self.maxRows))`. Loads since the request: "
                + "\(Array(bounded.loadHistory.dropFirst(loadsBeforeBound))). The caller has no "
                + "in-band way to learn its request was clamped, which is what makes an infinite "
                + "scroll spin forever."
        )
        XCTAssertEqual(
            bounded.latest.count, Int(Self.maxRows),
            "the window holds \(bounded.latest.count) rows after a request for \(beyond) against a "
                + "declared max of \(Self.maxRows). \(Self.historyCount) matching events exist, so "
                + "an unbounded window would show it here."
        )
        assertPage(
            bounded.latest.map(\.id), expectedPrefixOf: expectedOrder, target: Int(Self.maxRows),
            previous: previous, step: "at bound"
        )

        // --- A live event at a full window ---------------------------------
        //
        // The other direction of "no unbounded growth": new history arrives
        // while the app is already holding a full window. The newest-first
        // window must take the new row at the front and DROP the oldest one,
        // not become one row bigger.

        let live = try NostrSigning.sign(
            keyPair: keyPair, kind: 1, content: "C6 live, newest of all",
            createdAt: newest + 5
        )
        let droppedTail = expectedOrder[Int(Self.maxRows) - 1]
        let liveAccepted = try await relay.seed(live)
        XCTAssertTrue(liveAccepted, "the relay refused the live event")
        let liveArrived = await waitUntil(timeout: 45) {
            await ledger.current().latest.first?.id == live.id
        }
        try? await Task.sleep(nanoseconds: 500_000_000)
        let afterLive = await ledger.current()
        let afterLiveIDs = afterLive.latest.map(\.id)
        note(
            "live at a full window: arrived=\(liveArrived) rows=\(afterLiveIDs.count) "
                + "head=\(afterLiveIDs.first ?? "none") "
                + "stillHoldsDroppedTail=\(afterLiveIDs.contains(droppedTail)) "
                + "maxRowsEverHeld=\(afterLive.maxRowCount) batches=\(afterLive.batches)"
        )
        XCTAssertTrue(
            liveArrived,
            "a newer event published to a live relay never reached the head of the window: the "
                + "window holds \(afterLiveIDs.count) rows headed by "
                + "\(afterLiveIDs.first ?? "nothing"), ended \(afterLive.ended ?? "no"). A window "
                + "deep in history that stops seeing new events is a dead feed."
        )
        XCTAssertEqual(
            afterLiveIDs.count, Int(Self.maxRows),
            "the window grew to \(afterLiveIDs.count) rows when a live event arrived at an already "
                + "full window declared `max: \(Self.maxRows)`. This is the unbounded-growth "
                + "failure: an app scrolling a long feed accumulates every event it is ever sent."
        )
        XCTAssertEqual(
            afterLiveIDs, [live.id] + expectedOrder.prefix(Int(Self.maxRows) - 1),
            "the window after the live event must be the newest \(Self.maxRows) events in "
                + "canonical order -- the live one at the head and \(droppedTail) displaced off "
                + "the tail"
        )

        // --- Nothing anywhere in the run held more than it asked for -------

        XCTAssertNil(
            afterLive.overGrowthWitness,
            "a delivered batch carried more rows than the window target in force when it arrived: "
                + "\(afterLive.overGrowthWitness ?? "")"
        )
        XCTAssertNil(
            afterLive.duplicateWitness,
            "a delivered batch carried the same id twice: \(afterLive.duplicateWitness ?? "")"
        )
        XCTAssertEqual(
            afterLive.maxRowCount, Int(Self.maxRows),
            "the largest batch delivered across the whole scroll carried \(afterLive.maxRowCount) "
                + "rows against a declared max of \(Self.maxRows)"
        )

        consumer.cancel()
        query.cancel()
        try await relay.kill()
    }

    // MARK: - One page's worth of correctness

    /// Order, gaplessness, duplicate-freedom and stability, asserted for one
    /// delivered page. Split out because it is applied at every one of the
    /// ~10 advances, not because the assertions are being hidden -- each
    /// failure names the page it happened on and the exact ids involved.
    private func assertPage(
        _ ids: [String],
        expectedPrefixOf expectedOrder: [String],
        target: Int,
        previous: [String],
        step: String
    ) {
        let expected = Array(expectedOrder.prefix(target))
        XCTAssertEqual(
            Set(ids).count, ids.count,
            "\(step): the page carries a duplicated id -- \(ids)"
        )
        if ids != expected {
            let missing = Set(expected).subtracting(ids)
            let unexpected = Set(ids).subtracting(expected)
            let misordered = zip(ids, expected).enumerated().first { $0.element.0 != $0.element.1 }
            XCTFail(
                "\(step): the page is not the canonical newest-first prefix of the history. "
                    + "missing \(missing.count) id(s) \(missing.sorted().prefix(3)); unexpected "
                    + "\(unexpected.count) id(s) \(unexpected.sorted().prefix(3)); first position "
                    + "that differs: index \(misordered?.offset.description ?? "none") got "
                    + "\(misordered?.element.0 ?? "-") expected \(misordered?.element.1 ?? "-")"
            )
        }
        // Stability: growing the window must EXTEND the page, never rewrite
        // it. A correct final set can still have reshuffled under the app's
        // scroll position, and that is a real defect this catches.
        XCTAssertEqual(
            Array(ids.prefix(previous.count)), previous,
            "\(step): growing the window rewrote rows the app was already holding rather than "
                + "appending older ones. Previously held \(previous.count) rows; the new page's "
                + "first \(previous.count) are \(Array(ids.prefix(previous.count)).suffix(5)) "
                + "against \(previous.suffix(5))"
        )
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
}
