// canary-c17-churner: the half of C17 that is measured.
//
// C17 asks whether NMP's resource usage returns to a steady state under
// repeated open/close churn, or grows monotonically. Three of the four
// numbers that answer that question -- memory footprint, open file
// descriptors, live threads -- have no per-object equivalent on macOS.
// They are properties of a PROCESS.
//
// Issue #1796 is why that fact forces this executable to exist. Two
// existing idle-CPU tests read `getrusage` for the WHOLE PROCESS from
// inside a shared test binary, so any concurrent test in the same process
// can fail them; the measurement cannot tell "the thing under test
// misbehaved" from "something else in this process was busy". A leak
// oracle has the same exposure in both directions -- ambient allocation
// can manufacture growth that is not NMP's, and ambient release can hide
// growth that is.
//
// So the churn runs HERE, in a process whose entire job is the churn.
// Nothing else runs in it: no XCTest runner, no other scenario, no relay
// controller, no relay-lab dependency at all. "This process grew by N
// bytes" therefore means "the churn grew it by N bytes". The parent test
// (C17RepeatedLifecycleChurnTests.swift) starts the relay, seeds the one
// event over a real EVENT frame, spawns this, and reads the samples off
// stdout -- the same parent/child split C9 already uses for
// `canary-c9-publisher`, for a different reason.
//
// Public `NMP` API only: `import NMP`, no `@testable`, no internal crate,
// no Redb inspection. The one NMP-scoped resource number this samples --
// per-relay `wireSubCount` and the exact wire filters -- comes from the
// public `observeDiagnostics()` surface, which is the only resource count
// NMP exposes to an application at all.
//
// Usage:
//   canary-c17-churner <storePath> <phase> <cycles> <relay> <authorHex>
//   phase: "repeat" | "distinct" | "engine"
//
// Output, one line per completed cycle, in this exact field order:
//   SAMPLE:<cycle>,<footprintBytes>,<mallocBytesInUse>,<openFDs>,
//          <threads>,<wireSubCount>,<wireFilterCount>,<relayRowCount>
// plus PHASE:<name> at the start and, once every observation is closed,
//   DRAINED:<wireSubCount>,<openFDs>,<threads>

import Darwin
import Foundation
import NMP

// MARK: - Process resource sampling
//
// Every reading below is for THIS process. That is the point (see the
// header): this process does nothing but churn NMP.

/// `phys_footprint` -- the figure macOS itself charges a process against
/// its memory limit, and the one Activity Monitor shows as "Memory". Page
/// granular, so it cannot see a leak smaller than a page; `mallocBytesInUse`
/// below is the byte-granular companion that can.
func physFootprintBytes() -> UInt64 {
    var info = task_vm_info_data_t()
    var count = mach_msg_type_number_t(
        MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<natural_t>.size
    )
    let result = withUnsafeMutablePointer(to: &info) {
        $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
            task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), $0, &count)
        }
    }
    guard result == KERN_SUCCESS else { return 0 }
    return UInt64(info.phys_footprint)
}

/// Bytes currently handed out by malloc across every zone, from
/// `<malloc/malloc.h>`'s own `mstats()`. Byte granular, which is what makes
/// it the sharp oracle here: Rust's default allocator on macOS IS the
/// system malloc, so an engine-side heap allocation that is never freed
/// shows up in this number even when it is far too small to move the
/// page-granular footprint.
func mallocBytesInUse() -> UInt64 {
    UInt64(mstats().bytes_used)
}

/// Exact count of open file descriptors. `proc_pidinfo(PROC_PIDLISTFDS)`
/// with a zero-size buffer returns the kernel's OVER-estimate (45 for a
/// process holding 3), so this fills a real buffer and counts what came
/// back -- the difference between an exact oracle and a noisy one.
func openFileDescriptorCount() -> Int {
    let estimate = proc_pidinfo(getpid(), PROC_PIDLISTFDS, 0, nil, 0)
    guard estimate > 0 else { return -1 }
    let capacity = Int(estimate) / MemoryLayout<proc_fdinfo>.stride + 64
    var buffer = [proc_fdinfo](repeating: proc_fdinfo(), count: capacity)
    let filled = buffer.withUnsafeMutableBufferPointer { pointer -> Int32 in
        proc_pidinfo(
            getpid(), PROC_PIDLISTFDS, 0, pointer.baseAddress,
            Int32(capacity * MemoryLayout<proc_fdinfo>.stride)
        )
    }
    guard filled > 0 else { return -1 }
    return Int(filled) / MemoryLayout<proc_fdinfo>.stride
}

func liveThreadCount() -> Int32 {
    var info = proc_taskinfo()
    let size = MemoryLayout<proc_taskinfo>.size
    let result = proc_pidinfo(getpid(), PROC_PIDTASKINFO, 0, &info, Int32(size))
    guard result == Int32(size) else { return -1 }
    return info.pti_threadnum
}

// MARK: - The one NMP-scoped resource count the public API exposes
//
// `observeDiagnostics()` is PUSH-only: there is no synchronous "what is
// your current snapshot" call, so an application that wants a
// point-in-time resource reading has to hold the stream open and keep the
// last value. This box is exactly that, and nothing more -- it hides no
// NMP behaviour, it just remembers the newest snapshot the engine pushed.
// (`NMPDiagnosticsSnapshotObserver` is the shipped `@Observable` sugar for
// the same pattern; it is `@MainActor` and macOS 14+, neither of which
// suits a plain command-line churn loop.)
final class LatestDiagnostics: @unchecked Sendable {
    private let lock = NSLock()
    private var value = DiagnosticsSnapshot()

    func store(_ snapshot: DiagnosticsSnapshot) {
        lock.lock()
        value = snapshot
        lock.unlock()
    }

    func current() -> DiagnosticsSnapshot {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

/// The engine's own reported subscription count, summed over every relay
/// in the newest pushed snapshot.
func wireSubCount(_ diagnostics: LatestDiagnostics) -> UInt32 {
    diagnostics.current().relays.reduce(UInt32(0)) { $0 + $1.wireSubCount }
}

func emitSample(
    cycle: Int,
    openWireSubs: UInt32,
    openWireFilters: Int,
    closedWireSubs: UInt32,
    relayCount: Int
) {
    print(
        "SAMPLE:\(cycle),\(physFootprintBytes()),\(mallocBytesInUse()),"
            + "\(openFileDescriptorCount()),\(liveThreadCount()),"
            + "\(openWireSubs),\(openWireFilters),\(closedWireSubs),\(relayCount)"
    )
}

// MARK: - One churn cycle's read

/// Bounded wait for the engine's own reported wire-subscription count to
/// satisfy `condition`, returning whatever it actually was when the wait
/// ended. Never a sleep used AS the oracle: on timeout it returns the real
/// stuck number rather than pretending, and that number is what gets
/// sampled and asserted.
///
/// Waiting for `>= 1` before sampling is what makes a cycle a real cycle.
/// The obvious alternative -- wait for the query's first delivered batch --
/// does NOT work, and finding that out was the first real result of writing
/// this scenario: the first batch carries `rows=0` and arrives from the
/// local store immediately, long before the subscription reaches the relay
/// at all (measured: three consecutive `rows=0` batches precede the
/// `rows=1` one). A churn loop gated on the first batch tears the
/// observation down before it was ever established, and would have churned
/// nothing while looking green.
func awaitWireSubs(
    _ diagnostics: LatestDiagnostics,
    timeout: TimeInterval,
    condition: (UInt32) -> Bool
) async -> UInt32 {
    let deadline = Date().addingTimeInterval(timeout)
    var value = wireSubCount(diagnostics)
    while !condition(value), Date() < deadline {
        try? await Task.sleep(nanoseconds: 2_000_000)
        value = wireSubCount(diagnostics)
    }
    return value
}

func randomPubkeyHex() -> String {
    (0..<32).map { _ in String(format: "%02x", UInt8.random(in: 0...255)) }.joined()
}

// MARK: - Driver

@main
struct CanaryC17Churner {
    static func main() async {
        do {
            try await run()
        } catch {
            FileHandle.standardError.write(Data("FAILED: \(error)\n".utf8))
            print("FAILED:\(error)")
            exit(1)
        }
    }

    static func run() async throws {
        setbuf(stdout, nil)
        let args = CommandLine.arguments
        guard args.count == 6, let cycles = Int(args[3]) else {
            print("usage: canary-c17-churner <storePath> <phase> <cycles> <relay> <authorHex>")
            exit(2)
        }
        let storePath = args[1]
        let phase = args[2]
        let relay = args[4]
        let authorHex = args[5]

        print("PHASE:\(phase)")

        switch phase {
        case "repeat":
            try await churnOneEngine(
                storePath: storePath, relay: relay, cycles: cycles,
                filterFor: { NMPFilter(kinds: [1], authors: .literal([authorHex])) }
            )
        case "distinct":
            // A DIFFERENT wire filter every cycle, still matching the seeded
            // event so a batch still arrives. This is the shape that would
            // expose a per-subscription map keyed by filter that is never
            // pruned -- the "repeat" phase alone could hide such a map behind
            // deduplication of one identical filter.
            try await churnOneEngine(
                storePath: storePath, relay: relay, cycles: cycles,
                filterFor: {
                    NMPFilter(kinds: [1], authors: .literal([authorHex, randomPubkeyHex()]))
                }
            )
        case "engine":
            try await churnWholeEngine(
                storePath: storePath, relay: relay, cycles: cycles, authorHex: authorHex
            )
        default:
            print("FAILED:unknown phase \(phase)")
            exit(2)
        }
    }

    /// Phases "repeat" and "distinct": ONE engine, `cycles` open/close
    /// rounds of a live observation against it.
    static func churnOneEngine(
        storePath: String,
        relay: String,
        cycles: Int,
        filterFor: () -> NMPFilter
    ) async throws {
        let engine = try NMPEngine(config: NMPConfig(storePath: storePath, appRelays: [relay]))
        let diagnostics = LatestDiagnostics()
        let stream = try engine.observeDiagnostics()
        let pump = Task {
            do {
                for try await snapshot in stream {
                    diagnostics.store(snapshot)
                }
            } catch {}
        }

        for cycle in 0..<cycles {
            let query = try engine.observe(
                .single(NMPDemand(selection: filterFor()))
            )
            // Sampled WHILE the observation is still open and established on
            // the wire. Without this the post-teardown zero would be
            // indistinguishable from a diagnostics stream that never
            // delivered anything -- a zero that is always zero is not
            // evidence of teardown.
            let openWireSubs = await awaitWireSubs(diagnostics, timeout: 10) { $0 >= 1 }
            let openWireFilters = diagnostics.current().relays.reduce(0) { $0 + $1.filters.count }
            guard openWireSubs >= 1 else {
                print("FAILED:no wire subscription established at cycle \(cycle)")
                exit(1)
            }
            query.cancel()
            // Short and bounded: a teardown that does NOT release is the
            // finding, and it must arrive as a growing number in the sample
            // series, not as a hang.
            let closedWireSubs = await awaitWireSubs(diagnostics, timeout: 0.25) { $0 == 0 }
            emitSample(
                cycle: cycle, openWireSubs: openWireSubs, openWireFilters: openWireFilters,
                closedWireSubs: closedWireSubs,
                relayCount: diagnostics.current().relays.count
            )
        }

        // Every observation is now closed. Give the engine a bounded chance
        // to converge and report where it landed: this is the "does it come
        // back to a steady state" half, separate from the growth trend above.
        await settle(diagnostics: diagnostics)
        pump.cancel()
        stream.cancel()
        engine.shutdown()
    }

    /// Phase "engine": `cycles` rounds of constructing a whole engine over
    /// the SAME store path, reading through it once, and shutting it down.
    /// This is also the scenario's connect/disconnect churn -- each engine
    /// dials the relay fresh and each `shutdown()` drops it, so a leaked
    /// socket or a leaked engine thread per lifecycle shows up in the fd and
    /// thread series without needing a second control channel to the relay.
    static func churnWholeEngine(
        storePath: String,
        relay: String,
        cycles: Int,
        authorHex: String
    ) async throws {
        let filter = NMPFilter(kinds: [1], authors: .literal([authorHex]))
        // Declared outside the loop only so `settle` can read the LAST
        // engine's stream after the loop; each cycle replaces it with a
        // fresh box, because a snapshot pushed by an engine that no longer
        // exists is stale, not evidence.
        var diagnostics = LatestDiagnostics()

        for cycle in 0..<cycles {
            diagnostics = LatestDiagnostics()
            let engine = try NMPEngine(
                config: NMPConfig(storePath: storePath, appRelays: [relay])
            )
            let stream = try engine.observeDiagnostics()
            let pump = Task {
                do {
                    for try await snapshot in stream {
                        diagnostics.store(snapshot)
                    }
                } catch {}
            }
            let query = try engine.observe(
                .single(NMPDemand(selection: filter))
            )
            let openWireSubs = await awaitWireSubs(diagnostics, timeout: 10) { $0 >= 1 }
            let openWireFilters = diagnostics.current().relays.reduce(0) { $0 + $1.filters.count }
            let relayCount = diagnostics.current().relays.count
            query.cancel()
            // Read the post-teardown count while the engine is still ALIVE.
            // After `shutdown()` its stream is gone and the box would hold a
            // stale number that only looks like a released resource.
            let closedWireSubs = await awaitWireSubs(diagnostics, timeout: 0.25) { $0 == 0 }
            pump.cancel()
            stream.cancel()
            engine.shutdown()
            guard openWireSubs >= 1 else {
                print("FAILED:no wire subscription established at cycle \(cycle)")
                exit(1)
            }
            // Sampled AFTER `shutdown()` on purpose: the question this phase
            // asks is what a completed engine lifecycle leaves behind in the
            // process.
            emitSample(
                cycle: cycle, openWireSubs: openWireSubs, openWireFilters: openWireFilters,
                closedWireSubs: closedWireSubs, relayCount: relayCount
            )
        }

        await settle(diagnostics: diagnostics)
    }

    /// Bounded wait for the engine-scoped subscription count to return to
    /// zero once nothing is open, then report the final process numbers.
    /// Bounded, never a sleep used AS the oracle: if it never reaches zero
    /// the loop simply ends and `DRAINED:` reports the real number it was
    /// stuck at, which is the finding.
    static func settle(diagnostics: LatestDiagnostics) async {
        let deadline = Date().addingTimeInterval(5)
        var wireSubs = UInt32.max
        while Date() < deadline {
            wireSubs = diagnostics.current().relays.reduce(UInt32(0)) { $0 + $1.wireSubCount }
            if wireSubs == 0 { break }
            try? await Task.sleep(nanoseconds: 50_000_000)
        }
        print("DRAINED:\(wireSubs),\(openFileDescriptorCount()),\(liveThreadCount())")
    }
}
