// canary-c16-consumer: the SLOW READER half of C16 (#1869).
//
// C16 asks what NMP does when events arrive faster than the app reads
// them. The app under test is this process: it opens ONE ordinary
// `engine.observe(filter)` and iterates it with `for try await`, sleeping
// a fixed amount inside the loop body before pulling again. That sleep is
// the whole point -- it is not a synchronization oracle, it IS the slow
// consumer. The flood arrives from outside: the parent scenario publishes
// real events to a real strfry process over real EVENT frames.
//
// WHY THIS IS A SEPARATE PROCESS (issue #1796). Two of C16's numbers --
// peak memory and open file descriptors while a backlog exists -- are
// properties of a PROCESS with no per-object equivalent, and #1796 is this
// repository's standing proof that a process-wide measurement inside a
// shared test binary cannot tell the subject from everything else running
// beside it. Equally important, the CADENCE numbers would be corrupted the
// same way: "how many batches did the reader accept in ten seconds"
// measured inside an XCTest binary running other scenarios is a
// measurement of the machine's load, not of NMP. Here the process does one
// thing.
//
// The parent reads these lines off stdout as they are produced, not at
// EOF, because C16's precondition -- how many times the reader managed to
// read while the producer was still publishing -- is only checkable at the
// instant the flood ends.
//
// Public `NMP` API only: `import NMP`, no `@testable`, no internal crate,
// no Redb inspection.
//
// Usage:
//   canary-c16-consumer <storePath> <relay> <authorHex> <readDelayMs>
//                       <expectedRows> <deadlineSeconds>
//
// Output:
//   READY:<wireSubs>,<distinct>            once the subscription is
//                                          established on the wire
//   BATCH:<n>,<rows>,<distinct>,<new>,<dupWitness>
//                                          one line per DELIVERED batch;
//                                          `new` is how many event ids
//                                          this batch added that the
//                                          reader had never seen
//   SAMPLE:<footprint>,<malloc>,<fds>,<threads>
//                                          every 50ms, independent of the
//                                          reader's cadence (a slow reader
//                                          would otherwise sample its own
//                                          peak far too rarely)
//   DONE:<batches>,<latestRows>,<distinct>,<maxNewInOneBatch>,<reason>

import Darwin
import Foundation
import NMP

// MARK: - Process resource sampling (this process only -- see the header)

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

/// Byte-granular heap in use across every malloc zone. Rust's default
/// allocator on macOS IS the system malloc, so engine-side retention lands
/// here even when it is far too small to move the page-granular footprint.
func mallocBytesInUse() -> UInt64 {
    UInt64(mstats().bytes_used)
}

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

/// Two tasks write to stdout here (the reader loop and the sampler), so the
/// writes are serialized. Interleaved half-lines would corrupt the parent's
/// parse, and the parent parses these lines as they arrive.
let emitLock = NSLock()

func emit(_ line: String) {
    emitLock.lock()
    print(line)
    emitLock.unlock()
}

/// `observeDiagnostics()` is PUSH-only: `NMPEngine` exposes no synchronous
/// "what is your current snapshot" call, so anything wanting a
/// point-in-time reading must hold the stream open and keep the last value.
/// This box is exactly that. C13 and C17's churner each contain the same
/// nine lines; the duplication is deliberate (docs/internals/canary.md: "a
/// little duplication is preferable to hiding evidence") because it IS the
/// evidence that every scenario needing a current resource reading has to
/// build this itself.
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

    func wireSubCount() -> UInt32 {
        current().relays.reduce(UInt32(0)) { $0 + $1.wireSubCount }
    }
}

@main
struct CanaryC16Consumer {
    static func main() async {
        do {
            try await run()
        } catch {
            FileHandle.standardError.write(Data("FAILED: \(error)\n".utf8))
            emit("FAILED:\(error)")
            exit(1)
        }
    }

    static func run() async throws {
        setbuf(stdout, nil)
        let args = CommandLine.arguments
        guard args.count == 7,
            let readDelayMs = Int(args[4]),
            let expectedRows = Int(args[5]),
            let deadlineSeconds = Double(args[6])
        else {
            print(
                "usage: canary-c16-consumer <storePath> <relay> <authorHex> "
                    + "<readDelayMs> <expectedRows> <deadlineSeconds>"
            )
            exit(2)
        }
        let storePath = args[1]
        let relay = args[2]
        let authorHex = args[3]

        let engine = try NMPEngine(
            config: NMPConfig(storePath: storePath, appRelays: [relay])
        )
        let diagnostics = LatestDiagnostics()
        let diagnosticsStream = try engine.observeDiagnostics()
        let diagnosticsPump = Task {
            do {
                for try await snapshot in diagnosticsStream { diagnostics.store(snapshot) }
            } catch {}
        }

        // Sampled on its own clock, never on the reader's. A reader
        // deliberately sleeping a second per batch would otherwise take one
        // memory sample per second, which is far too coarse to see the peak
        // of a backlog being built up continuously by a producer this
        // process does not control.
        let sampler = Task {
            while !Task.isCancelled {
                emit(
                    "SAMPLE:\(physFootprintBytes()),\(mallocBytesInUse()),"
                        + "\(openFileDescriptorCount()),\(liveThreadCount())"
                )
                try? await Task.sleep(nanoseconds: 50_000_000)
            }
        }

        // One ordinary observation. Nothing here is windowed, paced, or
        // configured for backpressure in any way -- an app has no such knob,
        // and C16's question is what NMP does when the app simply reads
        // slowly.
        let query = try engine.observe(.single(NMPDemand(selection: NMPFilter(kinds: [1], authors: .literal([authorHex])))))

        // The subscription must be established ON THE WIRE before the parent
        // is told to flood. C17's first draft ended each cycle at the query's
        // first delivered batch, which is wrong for exactly this reason: the
        // first batches carry rows=0 and come from the local store long before
        // anything reaches the relay. A flood that begins before the
        // subscription exists is a flood into a store, not into a reader.
        let readyDeadline = Date().addingTimeInterval(20)
        var wireSubs = diagnostics.wireSubCount()
        while wireSubs < 1, Date() < readyDeadline {
            try? await Task.sleep(nanoseconds: 5_000_000)
            wireSubs = diagnostics.wireSubCount()
        }
        emit("READY:\(wireSubs),0")

        var distinct = Set<String>()
        var batches = 0
        var latestRows = 0
        var maxNewInOneBatch = 0
        var duplicateWitness = "none"
        var reason = "expected-rows-reached"
        let deadline = Date().addingTimeInterval(deadlineSeconds)

        do {
            for try await batch in query {
                batches += 1
                let ids = batch.rows.map(\.id)
                if Set(ids).count != ids.count, duplicateWitness == "none" {
                    duplicateWitness = "batch\(batches)"
                }
                let new = ids.filter { !distinct.contains($0) }.count
                maxNewInOneBatch = max(maxNewInOneBatch, new)
                distinct.formUnion(ids)
                latestRows = ids.count
                emit("BATCH:\(batches),\(latestRows),\(distinct.count),\(new),\(duplicateWitness)")

                if distinct.count >= expectedRows { break }
                if Date() >= deadline {
                    reason = "deadline"
                    break
                }
                // THE SLOW CONSUMER. Sleeping here, inside the loop body,
                // is what stops the next native pull from being issued --
                // which is the only lever an app has. There is no
                // app-facing backpressure knob to turn instead, and that
                // absence is the honest shape of the question.
                if readDelayMs > 0 {
                    try? await Task.sleep(nanoseconds: UInt64(readDelayMs) * 1_000_000)
                }
            }
        } catch {
            reason = "threw:\(error)"
        }
        if distinct.count < expectedRows, reason == "expected-rows-reached" {
            reason = "stream-ended-early"
        }

        emit(
            "DONE:\(batches),\(latestRows),\(distinct.count),\(maxNewInOneBatch),\(reason)"
        )
        sampler.cancel()
        query.cancel()
        diagnosticsPump.cancel()
        diagnosticsStream.cancel()
        engine.shutdown()
        exit(0)
    }
}
