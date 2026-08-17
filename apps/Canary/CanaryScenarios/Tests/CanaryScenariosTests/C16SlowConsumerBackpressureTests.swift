// C16 (docs/internals/canary.md "Scenario status", #1869): slow consumer
// and backpressure -- what NMP does when a real relay delivers events
// faster than the app reads them.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C7/C9/C13/C17.
// Public `NMP` API only: plain `import NMP`, no `@testable`, no internal
// crate, no direct Redb inspection. The relay is reached only over a real
// `ws://` URL to a separate process.
//
// THE THREE CLAIMS, AND WHY THE PRIMARY ONE IS A COUNT.
//
// `NMPQuery`'s own doc says "the engine mailbox conflates intermediate
// reducer emits for a slow observer", and `PullDriver.swift` says an app
// `next()` inherits "the engine's bounded latest-value mailbox instead of
// gaining a second Swift queue". Those are two halves of one claim -- the
// backlog is bounded, and it is bounded by CONFLATION -- and nothing
// tested either half.
//
// The obvious instrument is memory, and it is the wrong primary one. What
// an unbounded queue would retain here is reducer EMITS, and a few hundred
// retained delta frames is a few hundred kilobytes: below the noise floor
// of a process whose live engine already holds tens of megabytes, exactly
// the trap C17's 16 B/cycle first draft fell into. Worse, memory cannot be
// compared across different flood sizes at all, because N more events
// genuinely IS more durable data -- the store legitimately grows and the
// confound swamps the signal.
//
// So the primary oracle is a COUNT with no noise floor:
//
//   Number of batches DELIVERED to the reader, against number of events
//   PUBLISHED by the producer.
//
// A queue that retained every emit must hand every one of them to the
// reader before the stream can drain, so the reader would receive about
// one batch per published event -- roughly the same total however slowly
// it reads. A bounded latest-value mailbox conflates instead, so the
// reader receives a number of batches bounded by ITS OWN cadence, far
// below the producer's volume. That is an exact integer relationship, and
// it distinguishes the two designs directly.
//
// A count alone would be satisfied by an engine that simply THREW ROWS
// AWAY, which is why the second claim is asserted against the same run:
// after the flood the reader's LATEST snapshot must contain every single
// published event id. Conflation is allowed to lose intermediate
// snapshots; it is never allowed to lose a row. The two assertions
// together are the whole finding -- fewer deliveries AND no lost rows can
// only be conflation.
//
// THE FAST-READER CONTROL. "The slow reader received few batches" means
// nothing on its own: it could equally be that this engine emits few
// batches for anybody. So the identical run is repeated with the read
// delay set to zero, and the slow arm must have received materially fewer
// batches than the fast one. Without that control the primary assertion is
// unfalsifiable in the direction that matters.
//
// THE PRECONDITION IS ASSERTED, NOT ASSUMED, AND GETTING IT RIGHT WAS THIS
// SCENARIO'S FIRST RESULT. A test where the app kept up proves nothing
// about backpressure whatsoever, so the obvious precondition is "when the
// producer finished, the reader had not yet seen most of the events". That
// is FALSE here, measured: a reader pulling four times less often still
// held 389 of 401 ids at flood end. Conflation means a slow reader falls
// behind in NOTIFICATIONS, not in CONTENT, because each delivery is the
// whole current snapshot rather than the next item in a queue. Currency is
// therefore the wrong axis, and the assertions instead measure how many
// times the reader could READ during the flood, and how many unseen ids a
// single delivery handed it -- a directly measured backlog depth. (The
// parent reads the child's progress off a pipe, so its reading can lag the
// child's true progress by at most one delivery; that skew makes the
// precondition HARDER to pass, never easier.)
//
// WHERE THE MEASUREMENT LIVES (issue #1796). Peak memory, file descriptors
// and thread count are properties of a PROCESS. So is "how many batches
// did this reader accept in ten seconds", which inside a shared XCTest
// binary would measure the machine's load rather than NMP. The reader is
// therefore `canary-c16-consumer`, a process whose only job is to read
// slowly -- C9's parent/child split, for a fourth distinct reason. Unlike
// C17 the parent consumes that stdout INCREMENTALLY rather than at EOF,
// because the precondition above is only checkable at one instant.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C16SlowConsumerBackpressureTests: XCTestCase {
    // MARK: - Shape of one arm

    private struct ArmResult {
        let label: String
        let readDelayMs: Int
        let eventsPublished: Int
        let eventsAcked: Int
        let readyWireSubs: Int
        /// Distinct event ids the reader had been delivered at the instant
        /// the producer's last `OK` came back. The precondition number.
        let distinctAtFloodEnd: Int
        let batchesAtFloodEnd: Int
        let totalBatches: Int
        let finalDistinct: Int
        let finalLatestRows: Int
        let maxNewInOneBatch: Int
        let duplicateWitness: String
        let doneReason: String
        let exitStatus: Int32
        let peakMallocBytes: Double
        let peakFootprintBytes: Double
        let earlyMaxFds: Int
        let lateMaxFds: Int
        let earlyMaxThreads: Int
        let lateMaxThreads: Int
        let sampleCount: Int
        let rawTail: String
    }

    /// 400 events at one every 25ms is a ten-second flood: long enough for
    /// a one-second reader to be lapped forty times over, short enough that
    /// two arms plus their drains fit in well under a minute. The 25ms gap
    /// is a PRODUCER PACE, never a synchronization oracle -- nothing waits
    /// on it to decide anything.
    private static let floodCount = 400
    private static let producerGapMs: UInt64 = 25
    private static let slowReadDelayMs = 1000
    private static let fastReadDelayMs = 0

    // MARK: - The scenario

    func testASlowReaderIsConflatedRatherThanQueuedAndLosesNoRows() async throws {
        let binaryPath = try Self.locateStrfryBinary()
        let consumerPath = try Self.consumerBinaryURL()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c16-\(UUID().uuidString)")
        let relayDir = root.appendingPathComponent("relay")
        try FileManager.default.createDirectory(at: relayDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let relay = try await RelayHandle(name: "c16-relay", workDir: relayDir, binaryPath: binaryPath)
        try await relay.start()
        defer { Task { try? await relay.kill() } }

        // Two arms, identical in every respect except how fast the reader
        // reads. Each gets its own author and its own fresh store, so the
        // ingest work -- decode, verify, persist, index -- is byte for byte
        // the same in both and the ONLY difference is the read delay.
        let slow = try await runArm(
            label: "slow", readDelayMs: Self.slowReadDelayMs,
            relay: relay, root: root, consumerPath: consumerPath
        )
        let fast = try await runArm(
            label: "fast", readDelayMs: Self.fastReadDelayMs,
            relay: relay, root: root, consumerPath: consumerPath
        )

        print(Self.report(slow: slow, fast: fast))

        // --- Preconditions -------------------------------------------------

        XCTAssertEqual(
            slow.eventsAcked, slow.eventsPublished,
            "PRECONDITION: the relay acknowledged \(slow.eventsAcked) of "
                + "\(slow.eventsPublished) published events with a real OK. There was no flood "
                + "to be slow about."
        )
        XCTAssertGreaterThanOrEqual(
            slow.readyWireSubs, 1,
            "PRECONDITION: the reader's subscription was never established on the wire "
                + "(wireSubCount \(slow.readyWireSubs)), so the flood went into a store rather "
                + "than at a reader. C17's first draft failed exactly this way."
        )
        // THE preconditions, and getting them right was C16's first real
        // result. The obvious statement -- "the reader had seen fewer than
        // half the event ids when the producer finished" -- is FALSE here
        // and measuring it taught why: a reader pulling four times less
        // often is still 97% current, because every delivery is a complete
        // snapshot rather than the next item in a queue. Currency is
        // therefore the wrong axis for "was it slower". The right axis is
        // how many times it was able to READ, and how far behind it fell
        // between two reads.
        XCTAssertLessThan(
            slow.batchesAtFloodEnd, slow.eventsPublished / 8,
            "PRECONDITION: the slow reader accepted \(slow.batchesAtFloodEnd) deliveries while "
                + "the producer published \(slow.eventsPublished) events. It serviced roughly one "
                + "delivery per event, so it was never behind and nothing below is about "
                + "backpressure. Lower the producer gap (\(Self.producerGapMs)ms) or raise the "
                + "read delay (\(slow.readDelayMs)ms)."
        )
        // A real backlog genuinely formed. One delivery carrying N event ids
        // the reader had never seen means N events arrived while it was not
        // pulling -- a measured backlog depth, not an inference.
        XCTAssertGreaterThanOrEqual(
            slow.maxNewInOneBatch, 8,
            "PRECONDITION: the deepest backlog the slow reader was ever handed in one delivery "
                + "was \(slow.maxNewInOneBatch) previously unseen event ids. Below a handful "
                + "there was no meaningful queue behind it, whatever the delivery counts say."
        )
        XCTAssertGreaterThan(
            slow.maxNewInOneBatch, fast.maxNewInOneBatch,
            "PRECONDITION: the slow reader's deepest backlog (\(slow.maxNewInOneBatch) ids in "
                + "one delivery) was not deeper than the fast reader's "
                + "(\(fast.maxNewInOneBatch)). If reading slowly does not put a reader further "
                + "behind, the two arms are not actually different runs."
        )

        // --- Claim 1: the backlog is BOUNDED, as an exact count ------------

        XCTAssertLessThan(
            slow.totalBatches, slow.eventsPublished / 2,
            "the slow reader was delivered \(slow.totalBatches) batches for "
                + "\(slow.eventsPublished) published events. A mailbox that retained every "
                + "reducer emit would have to hand all of them to the reader before the stream "
                + "could drain, so this count would approach the published count however slowly "
                + "the app reads. This is that shape."
        )
        // The control. Without it, a low batch count could just mean the
        // engine emits rarely for everybody.
        XCTAssertLessThan(
            slow.totalBatches, fast.totalBatches,
            "the slow reader (\(slow.readDelayMs)ms per batch) was delivered "
                + "\(slow.totalBatches) batches and the fast reader (\(fast.readDelayMs)ms) "
                + "\(fast.totalBatches), against \(slow.eventsPublished) published events each. "
                + "If the two are the same the delivery count is a property of the engine's emit "
                + "rate rather than of the reader's cadence, and the boundedness assertion above "
                + "is measuring nothing."
        )
        // Not asserted, RECORDED, and it is C16's most useful number: how
        // current the slow reader still was at the instant the producer
        // finished. Conflation means the answer is "almost completely" --
        // the reader falls behind in NOTIFICATIONS, not in CONTENT, because
        // every delivery is the whole current snapshot rather than the next
        // item in a queue. Asserting a figure would freeze a ratio that
        // depends on the producer's rate; printing it is what makes the
        // design visible.
        print(
            "  C16 currency: at flood end the slow reader held "
                + "\(slow.distinctAtFloodEnd)/\(slow.eventsPublished + 1) event ids after only "
                + "\(slow.batchesAtFloodEnd) deliveries; the fast reader held "
                + "\(fast.distinctAtFloodEnd)/\(fast.eventsPublished + 1) after "
                + "\(fast.batchesAtFloodEnd)."
        )

        // --- Claim 2: nothing was silently DROPPED -------------------------
        //
        // The reader is expected to end holding the anchor, all 400 flooded
        // events, and the one published after the drain. Asserted on the
        // LATEST snapshot, not on the union of everything ever seen: a row
        // that appears and later vanishes is a loss the union would hide
        // (C13's lesson).

        let expected = slow.eventsPublished + 2
        XCTAssertEqual(
            slow.finalDistinct, expected,
            "the slow reader ended having seen \(slow.finalDistinct) distinct event ids against "
                + "\(expected) genuinely on the relay (1 anchor + \(slow.eventsPublished) "
                + "flooded + 1 published after the drain). Below that, NMP dropped events for a "
                + "reader that was merely slow. Reader's own reason for stopping: "
                + "\(slow.doneReason)."
        )
        XCTAssertEqual(
            slow.finalLatestRows, expected,
            "the slow reader's LATEST snapshot holds \(slow.finalLatestRows) rows against "
                + "\(expected) expected. Above it, a duplicate canonical row; below it, a row "
                + "that was delivered and then vanished."
        )
        XCTAssertEqual(
            slow.duplicateWitness, "none",
            "a delivered batch carried the same event id twice (\(slow.duplicateWitness))"
        )

        // --- Claim 3: it is not WEDGED -------------------------------------
        //
        // The last event is published AFTER the drain assertion, onto the
        // same query handle that was never reopened, and the reader's own
        // stop reason has to be "I saw everything I was told to expect"
        // rather than "my deadline expired". Then the process exits 0 of
        // its own accord.

        XCTAssertEqual(
            slow.doneReason, "expected-rows-reached",
            "the slow reader stopped because '\(slow.doneReason)' rather than because it had "
                + "been delivered every expected row. A reader that times out having consumed a "
                + "flood is a wedged pipeline, not a slow one."
        )
        XCTAssertEqual(
            slow.exitStatus, 0,
            "canary-c16-consumer exited \(slow.exitStatus) -- \(slow.rawTail)"
        )

        // --- Claim 4: no resource grew while the backlog existed -----------
        //
        // Exact integer counts, so no tolerance is warranted and none is
        // given: one socket or thread leaked per delivered batch under a
        // backlog would be a monotone series.

        XCTAssertLessThanOrEqual(
            slow.lateMaxFds, slow.earlyMaxFds,
            "open file descriptors grew while the reader was behind: early max "
                + "\(slow.earlyMaxFds), late max \(slow.lateMaxFds) over \(slow.sampleCount) samples"
        )
        XCTAssertLessThanOrEqual(
            slow.lateMaxThreads, slow.earlyMaxThreads,
            "live threads grew while the reader was behind: early max \(slow.earlyMaxThreads), "
                + "late max \(slow.lateMaxThreads) over \(slow.sampleCount) samples"
        )
        // Memory, as the SECONDARY oracle it is. The two arms did identical
        // ingest work over identical stores, so the only thing that can
        // separate their peaks is the backlog the slow one held. See the
        // bound's own doc for what it can and cannot resolve.
        XCTAssertLessThan(
            slow.peakMallocBytes - fast.peakMallocBytes, Self.backlogHeapBound,
            "the slow reader's peak heap exceeded the fast reader's by "
                + "\(UInt64(max(0, slow.peakMallocBytes - fast.peakMallocBytes))) bytes "
                + "(slow \(UInt64(slow.peakMallocBytes)) B, fast \(UInt64(fast.peakMallocBytes)) B) "
                + "for the same \(slow.eventsPublished) events over identical fresh stores. The "
                + "only difference between the two runs is how fast each read, so this is what "
                + "the backlog cost."
        )
    }

    /// How much more heap the slow arm may peak at than the fast arm, in
    /// bytes. MEASURED, not chosen -- the same discipline C17's 128 B/cycle
    /// bound was set with.
    ///
    /// The two arms are separate processes ingesting the identical 400
    /// events into identical fresh stores, so everything except the backlog
    /// is held constant and the difference between their peaks IS what the
    /// backlog cost. Observed at +51,920 / +77,712 / +96,608 / +97,520 B
    /// across four runs that pass every count assertion, against a ~76 MB
    /// live-engine baseline. The bound is 2 MB, roughly 21x the largest
    /// difference seen on a run known to be conflating properly.
    ///
    /// What it CAN see, and what makes 2 MB the right order of magnitude: a
    /// mailbox that retained one snapshot per event would hold 400 frames
    /// accumulating up to 400 rows each -- five orders of magnitude above
    /// this bound. What it CANNOT see is a bounded queue a few frames deep;
    /// that is kilobytes and this instrument has no hope of resolving it.
    /// The count assertions above are what actually decide the claim.
    private static let backlogHeapBound = 2.0 * 1024 * 1024

    // MARK: - One arm

    private func runArm(
        label: String,
        readDelayMs: Int,
        relay: RelayHandle,
        root: URL,
        consumerPath: URL
    ) async throws -> ArmResult {
        let storeDir = root.appendingPathComponent("store-\(label)")
        try FileManager.default.createDirectory(at: storeDir, withIntermediateDirectories: true)

        // A fresh author per arm, so the second arm's flood is genuinely
        // live traffic against its own subscription rather than a replay of
        // rows the relay already held for the first one.
        let keyPair = try NostrKeyPair()
        let anchor = try NostrSigning.sign(
            keyPair: keyPair, kind: 1, content: "C16 \(label) anchor"
        )
        try await relay.seed(anchor)

        let process = Process()
        process.executableURL = consumerPath
        process.arguments = [
            storeDir.appendingPathComponent("nmp.redb").path,
            relay.url,
            keyPair.pubkeyHex,
            String(readDelayMs),
            String(Self.floodCount + 2),
            "90",
        ]
        let pipe = Pipe()
        process.standardOutput = pipe
        let feed = ChildFeed()
        pipe.fileHandleForReading.readabilityHandler = { handle in
            feed.ingest(handle.availableData)
        }
        try process.run()
        defer {
            pipe.fileHandleForReading.readabilityHandler = nil
        }

        // The reader must be established on the wire before a single event
        // is published, or the flood lands in the store instead.
        let ready = await waitUntil(timeout: 40) { feed.snapshot().ready }
        let readySnapshot = feed.snapshot()
        guard ready else {
            await ChildProcess.killAndWaitForExit(process)
            throw ArmNeverEstablished(
                label: label,
                detail: "canary-c16-consumer never reported a live wire subscription -- "
                    + feed.tail()
            )
        }

        // The flood. `created_at` is stamped at signing time, and the
        // 25ms gap is a PRODUCER PACE, not a synchronization oracle: the
        // scenario never waits on it to decide anything.
        var floodEvents: [NostrEvent] = []
        floodEvents.reserveCapacity(Self.floodCount)
        for index in 0..<Self.floodCount {
            floodEvents.append(
                try NostrSigning.sign(
                    keyPair: keyPair, kind: 1, content: "C16 \(label) flood \(index)"
                )
            )
        }
        let acked = try await flood(events: floodEvents, to: relay.url, gapMs: Self.producerGapMs)

        // Read the reader's progress at the instant the producer finished.
        // This single number is the precondition.
        let atFloodEnd = feed.snapshot()

        // Drain, bounded. If NMP dropped events this never completes and
        // the real counts below are the finding -- it is not a hang.
        _ = await waitUntil(timeout: 60) {
            feed.snapshot().distinct >= Self.floodCount + 1
        }

        // One more event, on the same never-reopened query: the wedge check.
        let after = try NostrSigning.sign(keyPair: keyPair, kind: 1, content: "C16 \(label) after")
        try await relay.seed(after)

        let finished = await waitUntil(timeout: 60) { feed.snapshot().done != nil }
        let exited = await waitUntil(timeout: 20) { !process.isRunning }
        if !exited {
            await ChildProcess.killAndWaitForExit(process)
        }
        let final = feed.snapshot()
        _ = finished

        let samples = final.samples
        let split = samples.count / 2
        let early = Array(samples[..<split])
        let late = Array(samples[split...])

        return ArmResult(
            label: label,
            readDelayMs: readDelayMs,
            eventsPublished: Self.floodCount,
            eventsAcked: acked,
            readyWireSubs: readySnapshot.readyWireSubs,
            distinctAtFloodEnd: atFloodEnd.distinct,
            batchesAtFloodEnd: atFloodEnd.batches,
            totalBatches: final.done?.batches ?? final.batches,
            finalDistinct: final.done?.distinct ?? final.distinct,
            finalLatestRows: final.done?.latestRows ?? final.latestRows,
            maxNewInOneBatch: final.done?.maxNew ?? final.maxNew,
            duplicateWitness: final.duplicateWitness,
            doneReason: final.done?.reason ?? "(never reported DONE)",
            exitStatus: exited ? process.terminationStatus : -1,
            peakMallocBytes: samples.map(\.mallocBytes).max() ?? 0,
            peakFootprintBytes: samples.map(\.footprintBytes).max() ?? 0,
            earlyMaxFds: early.map(\.fds).max() ?? 0,
            lateMaxFds: late.map(\.fds).max() ?? 0,
            earlyMaxThreads: early.map(\.threads).max() ?? 0,
            lateMaxThreads: late.map(\.threads).max() ?? 0,
            sampleCount: samples.count,
            rawTail: feed.tail()
        )
    }

    // MARK: - The producer

    /// Publishes every event over ONE real WebSocket connection with real
    /// `OK`s collected concurrently, returning how many were accepted.
    /// `RelayHandle.seed` opens a fresh connection per event, which is the
    /// right shape for seeding one or two events and the wrong shape for
    /// four hundred: the connection setup would pace the producer instead
    /// of `gapMs`.
    private func flood(events: [NostrEvent], to url: String, gapMs: UInt64) async throws -> Int {
        guard let wsURL = URL(string: url) else { return 0 }
        let connection = WireConnection(url: wsURL)
        let ledger = AckLedger()
        let reader = Task {
            while !Task.isCancelled {
                guard let line = try? await connection.receiveLine(timeout: 30, what: "an OK")
                else { return }
                if case .ok(let id, let accepted, _) = RelayFrame.parse(line), accepted {
                    await ledger.record(id)
                }
            }
        }
        for event in events {
            try await connection.send(event.eventFrame())
            if gapMs > 0 {
                try await Task.sleep(nanoseconds: gapMs * 1_000_000)
            }
        }
        _ = await waitUntil(timeout: 30) { await ledger.count() >= events.count }
        let accepted = await ledger.count()
        reader.cancel()
        connection.close()
        return accepted
    }

    /// Thrown rather than `XCTSkip`ped: a scenario that cannot even
    /// establish its subject has not been skipped for a missing
    /// prerequisite, it has failed, and a skipped scenario is not a pass.
    private struct ArmNeverEstablished: Error, CustomStringConvertible {
        let label: String
        let detail: String
        var description: String { "C16 arm '\(label)': \(detail)" }
    }

    private actor AckLedger {
        private var ids = Set<String>()
        func record(_ id: String) { ids.insert(id) }
        func count() -> Int { ids.count }
    }

    // MARK: - Reading the child's stdout as it is produced

    private struct ChildSnapshot {
        var ready = false
        var readyWireSubs = 0
        var batches = 0
        var latestRows = 0
        var distinct = 0
        var maxNew = 0
        var duplicateWitness = "none"
        var samples: [ResourceSample] = []
        var done: DoneLine?
    }

    private struct ResourceSample {
        let footprintBytes: Double
        let mallocBytes: Double
        let fds: Int
        let threads: Int
    }

    private struct DoneLine {
        let batches: Int
        let latestRows: Int
        let distinct: Int
        let maxNew: Int
        let reason: String
    }

    /// The child's stdout, parsed line by line AS IT ARRIVES rather than at
    /// EOF (C17 reads to EOF, which is right for a run whose every number
    /// is final). C16's precondition is a reading taken at one instant --
    /// the moment the producer's last OK returns -- so the parent has to be
    /// able to ask "where is the reader right now".
    private final class ChildFeed: @unchecked Sendable {
        private let lock = NSLock()
        private var pending = Data()
        private var state = ChildSnapshot()
        private var recentLines: [String] = []

        func ingest(_ data: Data) {
            guard !data.isEmpty else { return }
            lock.lock()
            pending.append(data)
            while let index = pending.firstIndex(of: UInt8(ascii: "\n")) {
                let lineData = pending[pending.startIndex..<index]
                pending = pending[pending.index(after: index)...]
                if let line = String(data: Data(lineData), encoding: .utf8) {
                    apply(line)
                }
            }
            lock.unlock()
        }

        func snapshot() -> ChildSnapshot {
            lock.lock()
            defer { lock.unlock() }
            return state
        }

        func tail() -> String {
            lock.lock()
            defer { lock.unlock() }
            return recentLines.suffix(12).joined(separator: " | ")
        }

        /// Caller holds the lock.
        private func apply(_ line: String) {
            recentLines.append(line)
            if recentLines.count > 400 { recentLines.removeFirst(200) }

            if line.hasPrefix("READY:") {
                let fields = line.dropFirst("READY:".count).split(separator: ",").map(String.init)
                state.readyWireSubs = fields.first.flatMap(Int.init) ?? 0
                state.ready = true
            } else if line.hasPrefix("BATCH:") {
                let fields = line.dropFirst("BATCH:".count).split(separator: ",").map(String.init)
                guard fields.count == 5, let n = Int(fields[0]), let rows = Int(fields[1]),
                    let distinct = Int(fields[2]), let new = Int(fields[3])
                else { return }
                state.batches = n
                state.latestRows = rows
                state.distinct = distinct
                state.maxNew = max(state.maxNew, new)
                state.duplicateWitness = fields[4]
            } else if line.hasPrefix("SAMPLE:") {
                let fields = line.dropFirst("SAMPLE:".count).split(separator: ",").map(String.init)
                guard fields.count == 4, let footprint = Double(fields[0]),
                    let mallocBytes = Double(fields[1]), let fds = Int(fields[2]),
                    let threads = Int(fields[3])
                else { return }
                state.samples.append(
                    ResourceSample(
                        footprintBytes: footprint, mallocBytes: mallocBytes,
                        fds: fds, threads: threads
                    )
                )
            } else if line.hasPrefix("DONE:") {
                let fields = line.dropFirst("DONE:".count).split(separator: ",").map(String.init)
                guard fields.count >= 5, let batches = Int(fields[0]), let rows = Int(fields[1]),
                    let distinct = Int(fields[2]), let maxNew = Int(fields[3])
                else { return }
                state.done = DoneLine(
                    batches: batches, latestRows: rows, distinct: distinct,
                    maxNew: maxNew, reason: fields[4...].joined(separator: ",")
                )
            }
        }
    }

    // MARK: - Reporting

    /// Printed on every run, pass or fail. C16's value is the actual
    /// counts; a green checkmark with no numbers behind it would be exactly
    /// the vacuous pass the Canary exists to avoid.
    private static func report(slow: ArmResult, fast: ArmResult) -> String {
        var lines = [
            "",
            "C16: \(floodCount) events at one per \(producerGapMs)ms into two identical readers",
        ]
        func describe(_ arm: ArmResult) {
            lines.append("  arm '\(arm.label)' (\(arm.readDelayMs)ms per batch)")
            lines.append(
                "    published \(arm.eventsPublished), relay OK'd \(arm.eventsAcked), "
                    + "wireSubs at ready \(arm.readyWireSubs)"
            )
            lines.append(
                "    at flood end: reader had \(arm.distinctAtFloodEnd) distinct ids over "
                    + "\(arm.batchesAtFloodEnd) batches"
            )
            lines.append(
                "    at end: \(arm.totalBatches) batches total, \(arm.finalDistinct) distinct, "
                    + "\(arm.finalLatestRows) rows in the latest snapshot, "
                    + "max \(arm.maxNewInOneBatch) new ids in one batch"
            )
            lines.append(
                "    stopped because \(arm.doneReason), exit \(arm.exitStatus), "
                    + "duplicates: \(arm.duplicateWitness)"
            )
            lines.append(
                String(
                    format: "    peak malloc %.0f B, peak phys_footprint %.0f B over %d samples",
                    arm.peakMallocBytes, arm.peakFootprintBytes, arm.sampleCount
                )
            )
            lines.append(
                "    fds early max \(arm.earlyMaxFds) late max \(arm.lateMaxFds), "
                    + "threads early max \(arm.earlyMaxThreads) late max \(arm.lateMaxThreads)"
            )
        }
        describe(slow)
        describe(fast)
        lines.append(
            "  batches delivered: slow \(slow.totalBatches) vs fast \(fast.totalBatches) "
                + "for \(slow.eventsPublished) published events each"
        )
        lines.append(
            String(
                format: "  peak heap difference (slow - fast): %.0f B",
                slow.peakMallocBytes - fast.peakMallocBytes
            )
        )
        return lines.joined(separator: "\n")
    }

    // MARK: - Bounded polling

    /// Bounded poll on a real condition. The sleep paces the poll; it is
    /// never the thing being waited on, and the caller reports the real
    /// stuck values on `false`.
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

    // MARK: - Locating the two prerequisites

    private static func consumerBinaryURL() throws -> URL {
        let productsDir = Bundle(for: C16SlowConsumerBackpressureTests.self)
            .bundleURL.deletingLastPathComponent()
        let candidate = productsDir.appendingPathComponent("canary-c16-consumer")
        guard FileManager.default.isExecutableFile(atPath: candidate.path) else {
            throw XCTSkip("canary-c16-consumer not found next to the test bundle at \(candidate.path)")
        }
        return candidate
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
