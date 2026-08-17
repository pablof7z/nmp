// canary-c18-quitter: the app that quits, in C18 (#1870).
//
// C18 claims that when an app quits, everything stops: sockets close,
// threads exit, nothing is left running, nothing durable is left
// half-written. Almost none of that is observable from inside the process
// that is supposed to stop -- a process that failed to terminate is
// precisely the one that cannot report having failed. So this executable
// does the quitting and the PARENT scenario does the observing.
//
// This is the fourth distinct reason this suite splits parent from child,
// and it is worth naming against the other three because they are not
// interchangeable: C9's is that `kill -9` proves nothing against an
// in-process `Engine` drop; C2's is that a store read inside the process
// that just filled it is not a cold read; C17's and C16's is #1796, that a
// process-wide measurement inside a shared test binary is not an oracle.
// C18's is simpler and stronger than all three: exit status, termination
// reason, and "the OS process is gone" are facts about a process that only
// something outside it can state.
//
// WHAT MAKES THE QUIT A REAL QUIT. The interesting risk is not "does a
// Swift program reach the end of main" -- returning from `@main` calls
// `exit`, which would terminate the process whatever state NMP was left
// in. The risk is that `shutdown()` NEVER RETURNS, because it joins an
// engine thread that will not stop; that is what a wedged teardown
// actually looks like, and the parent's bound on the time from QUIT to
// exit is what catches it. The second risk is that a process which exits
// through `exit()` with Redb work in flight leaves durable state the next
// launch cannot read, which is why this app deliberately has real durable
// state to lose: a persisted local-key account and one really published,
// really signed event.
//
// AND THE APP IS GENUINELY BUSY WHEN IT QUITS. Nothing here is cancelled
// before the quit: the observation is still open, its consuming task is
// still running, the diagnostics stream is still open, and the relay
// socket is still connected. An app that tidied everything up first and
// then called `shutdown()` on an idle engine would prove nothing about
// clean shutdown, so this one does not.
//
// TWO MODES, and the difference between them is a real product claim.
// `explicit` calls `engine.shutdown()` -- twice, because its own doc says
// idempotent. `implicit` never calls it at all and relies on the `deinit`
// safety net `Engine.swift` advertises ("an app that forgets to call this
// explicitly does not leak the engine thread"). Nothing tested that.
//
// Public `NMP` API only: `import NMP`, no `@testable`, no internal crate,
// no Redb inspection.
//
// Usage:
//   canary-c18-quitter <storePath> <relay> <authorHex> <explicit|implicit>
//
// Output:
//   BASELINE:<fds>,<threads>
//        -- taken BEFORE the engine exists (and after Swift's own
//           concurrency pool has been warmed, so the pool is counted on
//           both sides). Everything else is compared against this.
//   LIVE:<wireSubs>,<rows>,<rowsSourcedByRelay>,<signedEventId>,<fds>,
//        <threads>,<accountHex>
//        -- printed once the app is genuinely running: subscribed on the
//           wire, holding rows the RELAY served, and holding one accepted,
//           signed write of its own. Then it waits for a QUIT on stdin.
//   QUITTING:<mode>          the QUIT line arrived and teardown begins
//   TORNDOWN:<fds>,<threads> teardown returned (this line NOT arriving is
//                            what a wedged `shutdown()` looks like)
//   RELEASED:<fds>,<threads> taken after the LAST engine reference was
//                            dropped and before the process ends -- the
//                            only window in which NMP's own teardown is
//                            distinguishable from the kernel's
// The process then returns from main. Anything after that is the parent's
// to observe.

import Darwin
import Foundation
import NMP

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

/// `observeDiagnostics()` is PUSH-only -- see the identical note in C13 and
/// in `canary-c17-churner`. Duplicated on purpose: the duplication is the
/// evidence that every scenario needing a current resource reading has to
/// build this box itself.
final class LatestDiagnostics: @unchecked Sendable {
    private let lock = NSLock()
    private var value = DiagnosticsSnapshot()

    func store(_ snapshot: DiagnosticsSnapshot) {
        lock.lock()
        value = snapshot
        lock.unlock()
    }

    func wireSubCount() -> UInt32 {
        lock.lock()
        defer { lock.unlock() }
        return value.relays.reduce(UInt32(0)) { $0 + $1.wireSubCount }
    }
}

/// What the still-running observation has been delivered. Read by the
/// liveness check below while the consuming task keeps writing to it --
/// the task is never stopped, because the app has to be busy when it quits.
final class ObservedRows: @unchecked Sendable {
    private let lock = NSLock()
    private var ids: [String] = []
    private var sourcedByRelay = 0

    func store(_ batch: RowBatch, relay: String) {
        lock.lock()
        ids = batch.rows.map(\.id)
        sourcedByRelay = batch.rows.filter { $0.sources.contains(relay) }.count
        lock.unlock()
    }

    func current() -> (count: Int, sourced: Int) {
        lock.lock()
        defer { lock.unlock() }
        return (ids.count, sourcedByRelay)
    }
}

/// The signed event id, written by the receipt-stream task and read by the
/// liveness check. That task is never stopped either.
final class SignedEventID: @unchecked Sendable {
    private let lock = NSLock()
    private var value: String?

    func store(_ id: String) {
        lock.lock()
        value = id
        lock.unlock()
    }

    func current() -> String? {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

@main
struct CanaryC18Quitter {
    static func main() async {
        setbuf(stdout, nil)
        let args = CommandLine.arguments
        guard args.count == 5 else {
            print("usage: canary-c18-quitter <storePath> <relay> <authorHex> <explicit|implicit>")
            exit(2)
        }
        // Swift's cooperative thread pool is created lazily on first use,
        // and this app uses `Task` heavily. A baseline taken before that
        // pool exists would charge Swift's own worker threads to NMP, so
        // the pool is warmed with real concurrent work FIRST and both
        // readings then count the same threads. Measured: this moves the
        // baseline from 1 thread to the pool's steady width of 15,
        // against a post-release reading of 15 -- without it that residue
        // looks like a leak and is not one.
        await withTaskGroup(of: Void.self) { group in
            for _ in 0..<max(8, ProcessInfo.processInfo.activeProcessorCount) {
                group.addTask {
                    var spin: UInt64 = 0
                    let until = Date().addingTimeInterval(0.05)
                    while Date() < until { spin &+= 1 }
                    _ = spin
                }
            }
        }

        // BEFORE the engine exists. Every later count is compared against
        // this, which is what turns "some resources were released" into
        // "the process is back where it started". The first draft asserted
        // only that the post-release counts were BELOW the live ones, and
        // deliberately leaking the engine into a global still satisfied
        // that -- releasing the query and diagnostics handles alone moved
        // 14 fds/19 threads to 11/17, which is strictly less than live and
        // nowhere near released. A baseline is not arbitrary and cannot be
        // half-satisfied.
        let baselineFds = openFileDescriptorCount()
        let baselineThreads = liveThreadCount()
        print("BASELINE:\(baselineFds),\(baselineThreads)")

        do {
            // `session` owns EVERY reference to the engine, so returning
            // from it is what releases the last one. That matters: in
            // `implicit` mode nothing calls `shutdown()`, so the only
            // teardown available is `NMPEngine.deinit`'s advertised safety
            // net, and the safety net cannot be observed at all unless the
            // engine reference is genuinely gone while the process is still
            // alive to report the numbers. Measuring after `session`
            // returns is the only place both are true.
            try await session(
                storePath: args[1], relay: args[2], authorHex: args[3], mode: args[4]
            )
            // Bounded settle back to the pre-engine file-descriptor count,
            // then the real numbers whatever they are. Bounded, never a
            // sleep used AS the oracle: on timeout it prints the number it
            // was stuck at, and that number is what the parent asserts on.
            // Three consuming tasks are still holding query, receipt and
            // diagnostics handles throughout -- exactly the state a
            // quitting app is in.
            let deadline = Date().addingTimeInterval(10)
            while openFileDescriptorCount() > baselineFds, Date() < deadline {
                try? await Task.sleep(nanoseconds: 50_000_000)
            }
            print("RELEASED:\(openFileDescriptorCount()),\(liveThreadCount())")
        } catch {
            print("FAILED:\(error)")
            exit(1)
        }
    }

    static func session(storePath: String, relay: String, authorHex: String, mode: String) async throws {
        let engine = try NMPEngine(config: NMPConfig(storePath: storePath, appRelays: [relay]))

        let diagnostics = LatestDiagnostics()
        let diagnosticsStream = try engine.observeDiagnostics()
        // Deliberately never cancelled. A quit that only works once every
        // stream has been torn down by hand is an app-side lifecycle hack,
        // which is one of the things the Canary exists to catch.
        Task {
            do {
                for try await snapshot in diagnosticsStream { diagnostics.store(snapshot) }
            } catch {}
        }

        // Real durable state to lose: a local-key account, made current, the
        // way `AppModel` signs in.
        let account = try engine.session.add(privateKey: .generate(), makeCurrent: true)
        let accountHex = account.publicKey.bytes.map { String(format: "%02x", $0) }.joined()

        // A live read. Also never cancelled, and its consuming task is still
        // running at quit time.
        let observed = ObservedRows()
        let query = try engine.observe(NMPFilter(kinds: [1], authors: .literal([authorHex])))
        Task {
            do {
                for try await batch in query { observed.store(batch, relay: relay) }
            } catch {}
        }

        // A real write of this app's own, published under that account to
        // the same relay. `publish` returning IS local acceptance, so by the
        // next line NMP owns a durable obligation -- exactly the state a
        // half-written store would corrupt.
        let receipt = try await engine.publish(
            WriteIntent(
                payload: .event(kind: 1, content: "C18 \(mode) note"),
                routing: .explicit(relays: [relay])
            )
        )

        // The write's own signed event id, off its receipt stream. Also
        // never cancelled. `publishQueue` is deliberately NOT the source
        // here: it is the record of what is OUTSTANDING, and a write to a
        // live relay settles and leaves it, so waiting on it would race.
        let signedID = SignedEventID()
        Task {
            do {
                for try await fact in receipt.status {
                    if case .signing(.signed(let eventID)) = fact { signedID.store(eventID) }
                }
            } catch {}
        }

        // The app is only "genuinely running" when all three are true at
        // once: subscribed on the wire, holding rows the RELAY actually
        // served (not rows that could have come from anywhere), and holding
        // its own signed write. Bounded; on timeout the real stuck numbers
        // are printed and the parent's preconditions fail on them.
        let deadline = Date().addingTimeInterval(30)
        var wireSubs = diagnostics.wireSubCount()
        var rows = observed.current()
        while Date() < deadline {
            wireSubs = diagnostics.wireSubCount()
            rows = observed.current()
            if wireSubs >= 1, rows.sourced >= 1, signedID.current() != nil { break }
            try? await Task.sleep(nanoseconds: 25_000_000)
        }
        print(
            "LIVE:\(wireSubs),\(rows.count),\(rows.sourced),\(signedID.current() ?? "none"),"
                + "\(openFileDescriptorCount()),\(liveThreadCount()),\(accountHex)"
        )

        // Wait until the parent says quit. The parent uses this window to
        // observe, from OUTSIDE, that this process is genuinely alive and
        // genuinely holds NMP's cross-process store lock (#489). The read
        // happens off the cooperative pool so a blocking `readLine` cannot
        // starve the three consuming tasks above -- they must still be
        // running when the quit arrives, or the app was not busy.
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            DispatchQueue.global().async {
                while let line = readLine(strippingNewline: true), line != "QUIT" {}
                continuation.resume()
            }
        }
        print("QUITTING:\(mode)")

        switch mode {
        case "explicit":
            // Twice, because `shutdown()`'s own doc says idempotent and
            // nothing tested it. A second call that hangs or traps would
            // stop TORNDOWN from ever being printed.
            engine.shutdown()
            engine.shutdown()
        case "implicit":
            // Nothing at all. `NMPEngine.deinit`'s safety net is the whole
            // claim under test in this mode.
            break
        default:
            print("FAILED:unknown mode \(mode)")
            exit(2)
        }

        print("TORNDOWN:\(openFileDescriptorCount()),\(liveThreadCount())")
        // Returning from here drops the last reference to `engine` (and to
        // `query`, `receipt` and the diagnostics stream, though the three
        // consuming tasks still hold their own). `main` measures again after
        // that and prints RELEASED. No `exit()` call anywhere on this path:
        // an explicit `exit()` would skip exactly the release the
        // `implicit` mode exists to exercise.
    }
}
