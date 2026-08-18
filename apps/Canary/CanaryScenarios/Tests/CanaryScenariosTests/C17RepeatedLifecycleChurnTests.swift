// C17 (docs/internals/canary.md "Scenario status"): repeated lifecycle
// churn -- whether NMP's resource usage returns to a steady state under
// many open/close cycles, or grows monotonically, against a real strfry
// child process.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C7/C9.
//
// WHY THE CHURN RUNS IN A CHILD PROCESS (issue #1796). Memory footprint,
// open file descriptors and live thread count have no per-object
// equivalent: they are properties of a PROCESS. #1796 is this repository's
// standing proof that a process-wide measurement taken inside a shared
// test binary is not an oracle -- two idle-CPU tests read whole-process
// `getrusage` and any concurrent test in the same binary can fail them,
// because the number cannot distinguish "the subject misbehaved" from
// "something else in this process was busy". A leak oracle is exposed in
// BOTH directions: ambient allocation manufactures growth NMP did not
// cause, and ambient release hides growth it did.
//
// So this file measures nothing itself. It starts the relay, seeds one
// event over a real EVENT frame, and spawns `canary-c17-churner` -- a
// process whose entire job is the churn, containing no XCTest runner, no
// other scenario and no relay controller. Every number below is therefore
// attributable to the churn by construction, not by hoping the test
// binary was quiet. The same parent/child split C9 uses, for a different
// reason.
//
// Public `NMP` API only on both sides: no `@testable`, no internal crate,
// no direct Redb inspection. The relay is reached only over a real `ws://`
// URL to a separate process.
//
// THE THRESHOLDS, AND HOW THEY WERE FIXED. The integer resources -- file
// descriptors, live threads, and the engine's own reported wire
// subscription count -- are asserted at ZERO growth. They are exact
// counts, so one leaked socket, thread or subscription per cycle is
// visible with no tolerance at all, and none is given.
//
// Heap bytes needed a resolution measurement rather than a guess, because
// a single live engine holds a ~74 MB heap (almost all of it the store's,
// released in full on `shutdown()` -- the "engine" phase measures 180 KB
// after teardown) and that baseline oscillates by tens of KB while the
// churn runs. The first draft asserted 16 B/cycle, malloc's smallest
// allocation quantum, on the theory that anything leaking one
// minimum-size allocation per cycle would exceed it. That number is below
// the instrument's noise floor and failed on a phase that provably does
// not leak, so it was not a threshold, it was a coin flip.
//
// The resolution was then measured directly, by running the same phase at
// 300 and at 1200 cycles and comparing the FINAL heap -- a per-cycle leak
// scales with the cycle count, a fixed cache does not:
//
//   repeat,   300 cycles: 74,456,848 B      1200 cycles: 74,456,400 B
//     -> -0.5 bytes per additional cycle. No per-cycle growth at all.
//   distinct, 300 cycles: 74,577,904 B      1200 cycles: 74,840,160 B
//     -> +291 bytes per additional distinct filter.
//
// Within a single run, the repeat phase (proven above to have zero
// per-cycle growth) still reported +22 B/cycle at 300 and +41 B/cycle at
// 1200 from the two-window average. That drift IS the noise floor, so the
// committed bound is 128 B/cycle -- roughly three times the largest drift
// observed on a series known to be flat. It is an instrument resolution,
// derived from a phase whose true answer was established independently;
// it was not raised until something went green. Stated plainly: this
// scenario cannot resolve a heap leak smaller than about 128 bytes per
// cycle in the single-engine phases. Footprint is bounded on TOTAL drift
// instead of a rate, for the reason given at its assertion.
//
// Set `C17_CYCLES` to re-run any phase at a different length; that is the
// lever the cross-length comparison above was made with, and it is the
// only way to tell a per-cycle leak from a one-time cost.
//
// Every measured value is printed on every run, pass or fail, so the
// evidence is in the log either way.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C17RepeatedLifecycleChurnTests: XCTestCase {
    // MARK: - What one cycle reported

    private struct Sample {
        let cycle: Int
        let footprintBytes: Double
        let mallocBytes: Double
        let openFileDescriptors: Int
        let threads: Int
        /// The engine's own `wireSubCount`, sampled WHILE this cycle's
        /// observation was still open.
        let openWireSubCount: Int
        let openWireFilterCount: Int
        /// The same count, sampled after the observation was cancelled.
        let closedWireSubCount: Int
        let relayCount: Int
    }

    private struct ChurnRun {
        let samples: [Sample]
        /// `DRAINED:<wireSubCount>,<openFDs>,<threads>` -- the state after
        /// every observation was closed.
        let drainedWireSubs: Int
        let drainedFileDescriptors: Int
        let drainedThreads: Int
        let exitStatus: Int32
        let rawTail: String
    }

    // MARK: - The three phases

    /// The identical filter, opened and closed 300 times against one
    /// engine. The cleanest steady-state question there is: churn the same
    /// thing repeatedly and the process should come back to the same place
    /// every time.
    func testRepeatedIdenticalObservationReturnsToSteadyState() async throws {
        let run = try await churn(phase: "repeat", cycles: 300)
        assertNoMonotonicGrowth(run, phase: "repeat")
    }

    /// A DIFFERENT wire filter every cycle (a second, random author added
    /// to the literal set, so each filter is distinct on the wire but still
    /// matches the seeded event). This is the shape that exposes
    /// per-subscription bookkeeping keyed by filter that is never pruned --
    /// something the identical-filter phase could hide behind deduplication.
    func testThreeHundredDistinctObservationsDoNotGrowMonotonically() async throws {
        let run = try await churn(phase: "distinct", cycles: 300)
        assertNoMonotonicGrowth(run, phase: "distinct")
    }

    /// Whole-engine churn: construct, read once, `shutdown()`, 60 times
    /// over the same store path. This is also C17's connect/disconnect
    /// churn -- every engine dials the relay fresh and every shutdown drops
    /// it -- so a leaked socket or a leaked engine thread per lifecycle
    /// lands in the file-descriptor and thread series directly.
    func testWholeEngineConstructAndShutdownChurnReleasesEverything() async throws {
        let run = try await churn(phase: "engine", cycles: 60)
        assertNoMonotonicGrowth(run, phase: "engine")
    }

    // MARK: - The oracle

    private func assertNoMonotonicGrowth(_ run: ChurnRun, phase: String) {
        XCTAssertEqual(run.exitStatus, 0, "canary-c17-churner exited \(run.exitStatus): \(run.rawTail)")
        guard run.samples.count >= 16 else {
            XCTFail("phase \(phase): only \(run.samples.count) samples parsed -- \(run.rawTail)")
            return
        }

        // Discard the first quarter as warm-up. Dialing the relay, opening
        // Redb, spinning up the Rust runtime's threads and filling the
        // allocator's first arenas are one-time costs; they are real, but
        // they are not the per-cycle growth C17 is asking about. The
        // remaining three quarters are split in half, and the question is
        // whether the second half sits where the first half did.
        let warmup = run.samples.count / 4
        let steady = Array(run.samples[warmup...])
        let split = steady.count / 2
        let early = Array(steady[..<split])
        let late = Array(steady[split...])

        print(Self.report(run: run, phase: phase, warmup: warmup, early: early, late: late))

        // 1-3: exact integer resources, asserted at zero growth. One leaked
        // socket, thread or wire subscription per cycle is a monotone
        // integer series, so no tolerance is warranted and none is given.
        XCTAssertLessThanOrEqual(
            late.map(\.openFileDescriptors).max() ?? 0,
            early.map(\.openFileDescriptors).max() ?? 0,
            "phase \(phase): open file descriptors grew across the run -- "
                + "early max \(early.map(\.openFileDescriptors).max() ?? 0), "
                + "late max \(late.map(\.openFileDescriptors).max() ?? 0), "
                + "first sample \(run.samples[0].openFileDescriptors), "
                + "last sample \(run.samples[run.samples.count - 1].openFileDescriptors)"
        )
        XCTAssertLessThanOrEqual(
            late.map(\.threads).max() ?? 0,
            early.map(\.threads).max() ?? 0,
            "phase \(phase): live threads grew across the run -- "
                + "early max \(early.map(\.threads).max() ?? 0), "
                + "late max \(late.map(\.threads).max() ?? 0), "
                + "first sample \(run.samples[0].threads), "
                + "last sample \(run.samples[run.samples.count - 1].threads)"
        )
        // The engine's own count, in two halves. The post-teardown series is
        // the leak oracle; the while-open series is what keeps it from being
        // vacuous. A number that reads zero after every teardown proves
        // nothing if it also read zero while the subscription was open --
        // that is a dead stream, not a released resource.
        let sawLiveSubscription = run.samples.contains { $0.openWireSubCount > 0 }
        XCTAssertTrue(
            sawLiveSubscription,
            "phase \(phase): the engine's diagnostics never reported a single wire "
                + "subscription while an observation was OPEN, across \(run.samples.count) "
                + "cycles. The post-teardown zeros below are therefore not evidence of "
                + "anything -- this oracle is dead, not passing."
        )
        XCTAssertLessThanOrEqual(
            late.map(\.closedWireSubCount).max() ?? 0,
            early.map(\.closedWireSubCount).max() ?? 0,
            "phase \(phase): the engine's own reported wire subscription count after "
                + "teardown grew across the run -- early max "
                + "\(early.map(\.closedWireSubCount).max() ?? 0), late max "
                + "\(late.map(\.closedWireSubCount).max() ?? 0). Closed observations are "
                + "not releasing their wire subscriptions."
        )

        // 4: heap bytes, at the instrument's measured resolution (see the
        // file header for how 128 was established and what it cannot see).
        let mallocPerCycle = Self.growthPerCycle(early: early.map(\.mallocBytes), late: late.map(\.mallocBytes))
        XCTAssertLessThan(
            mallocPerCycle, Self.heapBytesPerCycleBound,
            "phase \(phase): heap in use grew \(String(format: "%.1f", mallocPerCycle)) bytes per "
                + "cycle across \(steady.count) post-warm-up cycles (early mean "
                + "\(UInt64(Self.mean(early.map(\.mallocBytes)))) B, late mean "
                + "\(UInt64(Self.mean(late.map(\.mallocBytes)))) B, first sample "
                + "\(UInt64(run.samples[0].mallocBytes)) B, last sample "
                + "\(UInt64(run.samples[run.samples.count - 1].mallocBytes)) B). The bound is "
                + "this measurement's resolution, established by running a phase with proven "
                + "zero per-cycle growth; exceeding it means the open/close cycle itself "
                + "retains heap. Re-run with C17_CYCLES set 4x higher and compare THIS "
                + "B/cycle rate, not total heap: a per-cycle leak HOLDS the same rate, a "
                + "one-time cost falls to a quarter of it, and noise collapses toward zero "
                + "and can change sign. (The rate's divisor is the window span 3n/8, which "
                + "grows with run length -- so a real leak's numerator grows with it and "
                + "cancels. Total heap does end 4x higher; this number does not.)"
        )

        // 5: footprint, as a TOTAL drift bound rather than a per-cycle rate.
        // This is the figure macOS charges a process against its memory
        // limit, so it is the number that decides whether a long-lived app
        // eventually gets killed -- worth an assertion of its own. But it is
        // page granular and the OS reclaims pages in bursts: measured swings
        // of +-900 KB between the two windows on runs with no per-cycle
        // growth at all, in both directions, at every run length tried. A
        // per-cycle rate divides that fixed swing by the run length, so the
        // same behaviour reads as -33,810 B/cycle at 60 cycles and -3,166 at
        // 300 -- a bound on the rate is a coin flip that gets luckier the
        // longer the run. A bound on the TOTAL drift is length-independent
        // and sits above the measured noise.
        let footprintDrift = Self.mean(late.map(\.footprintBytes))
            - Self.mean(early.map(\.footprintBytes))
        XCTAssertLessThan(
            footprintDrift, Self.footprintDriftBound,
            "phase \(phase): phys_footprint drifted up by \(UInt64(footprintDrift)) bytes across "
                + "the measured window (early mean \(UInt64(Self.mean(early.map(\.footprintBytes)))) B, "
                + "late mean \(UInt64(Self.mean(late.map(\.footprintBytes)))) B, first sample "
                + "\(UInt64(run.samples[0].footprintBytes)) B, last sample "
                + "\(UInt64(run.samples[run.samples.count - 1].footprintBytes)) B) -- well past the "
                + "+-900 KB this measurement swings by on its own"
        )

        // 6: with nothing open, the engine's own count is back to zero.
        // This is the direct "returns to a steady state" claim, as opposed
        // to the trend assertions above.
        XCTAssertEqual(
            run.drainedWireSubs, 0,
            "phase \(phase): after every observation was closed the engine still reported "
                + "\(run.drainedWireSubs) wire subscription(s) after a bounded 5s wait"
        )
    }

    /// Bytes of heap growth per churn cycle this measurement can actually
    /// resolve. See the file header: measured, not chosen.
    private static let heapBytesPerCycleBound = 128.0

    /// Total upward footprint drift allowed across the measured window --
    /// 2 MB, roughly 2.3x the +-900 KB this page-granular figure swings by
    /// on runs proven to have no per-cycle growth. See the assertion.
    private static let footprintDriftBound = 2_097_152.0

    private static func mean(_ values: [Double]) -> Double {
        values.isEmpty ? 0 : values.reduce(0, +) / Double(values.count)
    }

    /// Growth per cycle between two adjacent equal-ish windows. The means
    /// sit at the windows' midpoints, so the distance between them is half
    /// the total span -- dividing by the raw window length would understate
    /// the rate by 2x.
    private static func growthPerCycle(early: [Double], late: [Double]) -> Double {
        let span = Double(early.count + late.count) / 2
        guard span > 0 else { return 0 }
        return (mean(late) - mean(early)) / span
    }

    /// Printed on every run, pass or fail. C17's whole value is the actual
    /// numbers; a green checkmark with no numbers behind it would be
    /// exactly the vacuous pass this scenario exists to avoid.
    private static func report(
        run: ChurnRun, phase: String, warmup: Int, early: [Sample], late: [Sample]
    ) -> String {
        var lines = ["", "C17 phase '\(phase)': \(run.samples.count) cycles, \(warmup) discarded as warm-up"]
        func row(_ name: String, _ early: [Double], _ late: [Double], _ first: Double, _ last: Double) {
            lines.append(
                String(
                    format: "  %-24s early %14.0f  late %14.0f  per-cycle %+10.2f  (first %.0f, last %.0f)",
                    (name as NSString).utf8String!, mean(early), mean(late),
                    growthPerCycle(early: early, late: late), first, last
                )
            )
        }
        let first = run.samples[0]
        let last = run.samples[run.samples.count - 1]
        row("phys_footprint bytes", early.map(\.footprintBytes), late.map(\.footprintBytes),
            first.footprintBytes, last.footprintBytes)
        row("malloc bytes in use", early.map(\.mallocBytes), late.map(\.mallocBytes),
            first.mallocBytes, last.mallocBytes)
        row("open file descriptors", early.map { Double($0.openFileDescriptors) },
            late.map { Double($0.openFileDescriptors) },
            Double(first.openFileDescriptors), Double(last.openFileDescriptors))
        row("live threads", early.map { Double($0.threads) }, late.map { Double($0.threads) },
            Double(first.threads), Double(last.threads))
        row("wireSubCount while open", early.map { Double($0.openWireSubCount) },
            late.map { Double($0.openWireSubCount) },
            Double(first.openWireSubCount), Double(last.openWireSubCount))
        row("wire filters while open", early.map { Double($0.openWireFilterCount) },
            late.map { Double($0.openWireFilterCount) },
            Double(first.openWireFilterCount), Double(last.openWireFilterCount))
        row("wireSubCount after close", early.map { Double($0.closedWireSubCount) },
            late.map { Double($0.closedWireSubCount) },
            Double(first.closedWireSubCount), Double(last.closedWireSubCount))
        lines.append(
            "  cycles that saw a live wire subscription while open: "
                + "\(run.samples.filter { $0.openWireSubCount > 0 }.count)/\(run.samples.count)"
        )
        lines.append(
            "  drained (nothing open): wireSubs \(run.drainedWireSubs), "
                + "fds \(run.drainedFileDescriptors), threads \(run.drainedThreads)"
        )
        // Deciles, because "grows monotonically" and "fills a bounded cache
        // and stops" produce the same two-window average. A plateau is
        // visible here and nowhere else in this report.
        let bucket = max(1, run.samples.count / 10)
        let deciles = stride(from: 0, to: run.samples.count, by: bucket).map { start -> String in
            let slice = run.samples[start..<min(start + bucket, run.samples.count)]
            return String(format: "%.0f", mean(slice.map(\.mallocBytes)))
        }
        lines.append("  malloc bytes by decile: " + deciles.joined(separator: " "))
        return lines.joined(separator: "\n")
    }

    // MARK: - Driving one phase

    private func churn(phase: String, cycles requested: Int) async throws -> ChurnRun {
        let cycles = ProcessInfo.processInfo.environment["C17_CYCLES"].flatMap(Int.init) ?? requested
        let binaryPath = try Self.locateStrfryBinary()
        let churnerPath = try Self.churnerBinaryURL()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c17-\(phase)-\(UUID().uuidString)")
        let workDir = root.appendingPathComponent("relay")
        let storeDir = root.appendingPathComponent("store")
        try FileManager.default.createDirectory(at: workDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: storeDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let relay = try await RelayHandle(name: "c17-\(phase)", workDir: workDir, binaryPath: binaryPath)
        try await relay.start()
        defer { Task { try? await relay.kill() } }

        // One event, seeded over a real EVENT frame before the churner
        // exists -- exactly C1's shape. Every cycle in the child then has
        // something real to deliver, which is what makes a cycle a cycle
        // rather than an observe that was torn down before it reached the
        // wire.
        let keyPair = try NostrKeyPair()
        let subject = try NostrSigning.sign(keyPair: keyPair, kind: 1, content: "C17 churn subject")
        try await relay.seed(subject)

        let process = Process()
        process.executableURL = churnerPath
        process.arguments = [
            storeDir.appendingPathComponent("nmp.redb").path,
            phase,
            String(cycles),
            relay.url,
            keyPair.pubkeyHex,
        ]
        let pipe = Pipe()
        process.standardOutput = pipe
        try process.run()

        // Read to EOF on a background queue, raced against a bounded
        // deadline that kills the child (which closes the pipe, so the read
        // returns rather than hanging).
        let output: String = await withTaskGroup(of: String?.self) { group in
            group.addTask {
                await withCheckedContinuation { continuation in
                    DispatchQueue.global().async {
                        let data = pipe.fileHandleForReading.readDataToEndOfFile()
                        continuation.resume(returning: String(data: data, encoding: .utf8) ?? "")
                    }
                }
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: 240_000_000_000)
                await ChildProcess.killAndWaitForExit(process)
                return nil
            }
            let result = await group.next() ?? nil
            group.cancelAll()
            return result ?? ""
        }
        await ChildProcess.killAndWaitForExit(process)

        var samples: [Sample] = []
        var drained = (subs: -1, fds: -1, threads: -1)
        for line in output.split(separator: "\n") {
            if line.hasPrefix("SAMPLE:") {
                let fields = line.dropFirst("SAMPLE:".count).split(separator: ",").map(String.init)
                guard fields.count == 9,
                    let cycle = Int(fields[0]), let footprint = Double(fields[1]),
                    let mallocBytes = Double(fields[2]), let fds = Int(fields[3]),
                    let threads = Int(fields[4]), let openSubs = Int(fields[5]),
                    let openFilters = Int(fields[6]), let closedSubs = Int(fields[7]),
                    let relays = Int(fields[8])
                else { continue }
                samples.append(
                    Sample(
                        cycle: cycle, footprintBytes: footprint, mallocBytes: mallocBytes,
                        openFileDescriptors: fds, threads: threads, openWireSubCount: openSubs,
                        openWireFilterCount: openFilters, closedWireSubCount: closedSubs,
                        relayCount: relays
                    )
                )
            } else if line.hasPrefix("DRAINED:") {
                let fields = line.dropFirst("DRAINED:".count).split(separator: ",").map(String.init)
                if fields.count == 3, let subs = Int(fields[0]), let fds = Int(fields[1]),
                    let threads = Int(fields[2]) {
                    drained = (subs, fds, threads)
                }
            }
        }

        return ChurnRun(
            samples: samples,
            drainedWireSubs: drained.subs,
            drainedFileDescriptors: drained.fds,
            drainedThreads: drained.threads,
            exitStatus: process.terminationStatus,
            rawTail: String(output.suffix(600))
        )
    }

    // MARK: - Locating the two prerequisites

    /// Same derivation C9 uses: SwiftPM puts every executable product and
    /// this test bundle in one build products directory, so derive the
    /// sibling binary from the bundle's own location rather than hardcoding
    /// a path shape. `CommandLine.arguments[0]` is the `xctest` runner here,
    /// not anything in this package.
    private static func churnerBinaryURL() throws -> URL {
        let productsDir = Bundle(for: C17RepeatedLifecycleChurnTests.self)
            .bundleURL.deletingLastPathComponent()
        let candidate = productsDir.appendingPathComponent("canary-c17-churner")
        guard FileManager.default.isExecutableFile(atPath: candidate.path) else {
            throw XCTSkip("canary-c17-churner not found next to the test bundle at \(candidate.path)")
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
