// C18 (docs/internals/canary.md "Scenario status", #1870): clean shutdown
// -- the app quits, and everything really stops.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C7/C9/C13/C16/C17.
// Public `NMP` API only: plain `import NMP`, no `@testable`, no internal
// crate, no direct Redb inspection. The relay is reached only over a real
// `ws://` URL to a separate process.
//
// TERMINATION IS THE CLAIM, NOT ABSENCE OF A CRASH. C17's `engine` phase
// already shows sixty construct/read/`shutdown()` cycles releasing their
// file descriptors and heap, and that is genuinely not this: that process
// stays alive for the next cycle, so a thread that never exits and a
// socket that never closes are still merely counted, and a store left
// half-written is never read back. Here the app process must be GONE --
// exited by itself, with status 0, within a bound, having been busy right
// up to the moment it was told to quit.
//
// WHERE THE RISK ACTUALLY IS. Returning from a Swift `@main` calls `exit`,
// which would terminate the process whatever state NMP was in, so "did the
// process end" is not by itself a hard question. The two hard ones are:
//
//   1. does teardown RETURN? `shutdown()` joins the engine; a teardown
//      that never comes back never reaches the return, and the process
//      hangs forever holding its store. The bound on QUIT-to-exit is what
//      catches that, and the child's own `TORNDOWN:` line separates
//      "teardown wedged" from "teardown finished but the process lingered".
//   2. is anything durable left HALF-WRITTEN? A process that exits through
//      `exit()` with Redb work in flight can leave a store the next launch
//      cannot use. So the app deliberately has real durable state to lose:
//      a persisted local-key account and one really signed, really
//      published event of its own.
//
// THE PRECONDITION IS ASSERTED FROM OUTSIDE, AND IT IS THE GOOD PART.
// "Prove the thing was genuinely running before asserting it stopped" is
// easy to fake from inside the process being tested. Two independent
// external facts are used instead:
//
//   - `lsof` reports this exact pid holding an ESTABLISHED TCP connection
//     to the relay's exact port. Not "NMP says it is connected" -- the
//     kernel's own answer;
//   - constructing an `NMPEngine` over the child's store path from THIS
//     process throws `NMPError.storeAlreadyOpen`. That is #489's
//     cross-process exclusive ownership lock, and it is a public-API fact
//     that this process cannot fake on the child's behalf.
//
// The second one is the sharper of the two, because the same call is the
// POST-condition: after the child exits it must SUCCEED. One mechanism
// gives both "it genuinely held the store" and "it genuinely let go".
//
// TWO MODES, and their difference is a real product claim. `explicit`
// calls `engine.shutdown()` (twice -- its own doc says idempotent, and
// nothing tested that). `implicit` never calls it at all, relying on the
// `deinit` safety net `Engine.swift` advertises: "an app that forgets to
// call this explicitly does not leak the engine thread". Nothing tested
// that either, and an app that must call `shutdown()` by hand or leak is a
// different product from one that must not.
//
// Every wait below is a bounded poll on a real condition with the real
// stuck value reported on timeout -- never a fixed sleep used AS the
// oracle. The child is SIGKILLed only on the failure path, and the
// assertion says so, because a process that had to be killed did not
// terminate.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C18CleanShutdownTests: XCTestCase {
    // MARK: - The two scenarios

    /// The ordinary shape: the app calls `shutdown()` and quits, with an
    /// open observation, an open diagnostics stream, three live consuming
    /// tasks and a connected relay socket.
    func testExplicitShutdownTerminatesWithWorkStillInFlight() async throws {
        try await runQuitScenario(mode: "explicit")
    }

    /// The advertised safety net: the app NEVER calls `shutdown()`. Same
    /// live state, same assertions. If this one needs an explicit call to
    /// terminate cleanly, `NMPEngine.deinit`'s documented promise is wrong.
    func testDeinitSafetyNetTerminatesWhenTheAppNeverCallsShutdown() async throws {
        try await runQuitScenario(mode: "implicit")
    }

    // MARK: - One run

    private func runQuitScenario(mode: String) async throws {
        let binaryPath = try Self.locateStrfryBinary()
        let quitterPath = try Self.quitterBinaryURL()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c18-\(mode)-\(UUID().uuidString)")
        let relayDir = root.appendingPathComponent("relay")
        let storeDir = root.appendingPathComponent("store")
        try FileManager.default.createDirectory(at: relayDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: storeDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let storePath = storeDir.appendingPathComponent("nmp.redb").path

        let relay = try await RelayHandle(name: "c18-\(mode)", workDir: relayDir, binaryPath: binaryPath)
        try await relay.start()

        // One event seeded over a real EVENT frame before the app exists, so
        // its read has something the RELAY genuinely served -- which is what
        // "the socket was really doing something" rests on.
        let keyPair = try NostrKeyPair()
        let seeded = try NostrSigning.sign(keyPair: keyPair, kind: 1, content: "C18 \(mode) seed")
        try await relay.seed(seeded)

        var log: [String] = []
        func note(_ line: String) { log.append(line) }
        defer { print((["", "C18 '\(mode)' phase log:"] + log).joined(separator: "\n")) }

        let process = Process()
        process.executableURL = quitterPath
        process.arguments = [storePath, relay.url, keyPair.pubkeyHex, mode]
        let stdout = Pipe()
        let stdin = Pipe()
        process.standardOutput = stdout
        process.standardInput = stdin
        let feed = ChildFeed()
        stdout.fileHandleForReading.readabilityHandler = { handle in
            feed.ingest(handle.availableData)
        }
        try process.run()
        let pid = process.processIdentifier
        defer {
            stdout.fileHandleForReading.readabilityHandler = nil
        }

        // --- Phase 1: the app is GENUINELY RUNNING -------------------------

        let reportedLive = await waitUntil(timeout: 60) { feed.snapshot().live != nil }
        guard reportedLive, let live = feed.snapshot().live else {
            await ChildProcess.killAndWaitForExit(process)
            note("never reached a live state: \(feed.tail())")
            return XCTFail(
                "PRECONDITION: canary-c18-quitter never reported a live state within 60s. "
                    + "There is nothing whose shutdown could be clean. Tail: \(feed.tail())"
            )
        }

        // The kernel's own answer, not NMP's: this pid holds an ESTABLISHED
        // TCP connection to this exact port.
        let established = Self.establishedConnections(pid: pid, port: relay.port)
        // NMP's own cross-process store lock (#489), read from a SECOND
        // process. This is the fact the child cannot fabricate.
        var lockHeld = false
        var lockError = "(constructed successfully -- the child does not hold the store)"
        do {
            let intruder = try NMPEngine(config: NMPConfig(storePath: storePath))
            intruder.shutdown()
        } catch let error as NMPError {
            if case .storeAlreadyOpen = error { lockHeld = true }
            lockError = "\(error)"
        }

        note(
            "live: pid=\(pid) wireSubs=\(live.wireSubs) rows=\(live.rows) "
                + "rowsSourcedByRelay=\(live.rowsSourced) signedEvent=\(live.signedEventID) "
                + "fds=\(live.fds) threads=\(live.threads) account=\(live.accountHex.prefix(12)) "
                + "establishedTCPToRelay=\(established) storeLockHeld=\(lockHeld) (\(lockError))"
        )
        XCTAssertGreaterThanOrEqual(
            live.wireSubs, 1,
            "PRECONDITION: the app reported \(live.wireSubs) wire subscriptions -- it never "
                + "subscribed, so there is no live subscription for the quit to close."
        )
        XCTAssertGreaterThanOrEqual(
            live.rowsSourced, 1,
            "PRECONDITION: \(live.rowsSourced) of the app's \(live.rows) rows name the relay in "
                + "their own `sources`. Without one the network never served this app anything, "
                + "and its socket was decorative."
        )
        XCTAssertNotEqual(
            live.signedEventID, "none",
            "PRECONDITION: the app never got a signed event id back for its own write, so there "
                + "is no durable write state for the shutdown to leave half-finished."
        )
        XCTAssertGreaterThanOrEqual(
            established, 1,
            "PRECONDITION: `lsof` reports \(established) ESTABLISHED TCP connections from pid "
                + "\(pid) to 127.0.0.1:\(relay.port). This is the kernel's answer, not NMP's: "
                + "with no real socket open there is no socket for the quit to close."
        )
        XCTAssertTrue(
            lockHeld,
            "PRECONDITION: constructing a second NMPEngine over \(storePath) from the test "
                + "process did not throw `storeAlreadyOpen` -- it reported \(lockError). #489's "
                + "cross-process ownership lock is what makes the post-exit release below mean "
                + "something, and if the child never held it, it cannot release it."
        )

        // --- Phase 2: quit, and the process must really END ----------------

        let quitAt = Date()
        try stdin.fileHandleForWriting.write(contentsOf: Data("QUIT\n".utf8))

        let tearDownReturned = await waitUntil(timeout: Self.terminationBudget) {
            feed.snapshot().tornDown != nil
        }
        let reportedRelease = await waitUntil(timeout: Self.terminationBudget) {
            feed.snapshot().released != nil
        }
        let exited = await waitUntil(timeout: Self.terminationBudget) { !process.isRunning }
        let elapsed = Date().timeIntervalSince(quitAt)
        let tornDown = feed.snapshot().tornDown
        let released = feed.snapshot().released
        let baseline = feed.snapshot().baseline
        if !exited {
            // ONLY on the failure path, and the assertion below says so. A
            // process that had to be killed did not terminate.
            await ChildProcess.killAndWaitForExit(process)
        }
        let status = process.terminationStatus
        let reason = process.terminationReason
        // The pid is genuinely gone: `kill(pid, 0)` fails with ESRCH. This
        // is separate from `Process.isRunning`, which is Foundation's own
        // bookkeeping about a child it reaped.
        let pidGone = kill(pid, 0) != 0 && errno == ESRCH

        note(
            String(
                format: "quit: exited=%@ in %.2fs, status=%d, reason=%@, pidGone=%@, "
                    + "torndown=%@, tail=%@",
                exited ? "yes" : "NO", elapsed, status,
                reason == .exit ? "exit" : "uncaughtSignal",
                pidGone ? "yes" : "no",
                tornDown.map { "fds \($0.fds)/threads \($0.threads)" } ?? "NEVER PRINTED",
                feed.tail()
            )
        )
        note(
            "resources: baseline "
                + (baseline.map { "fds \($0.fds)/threads \($0.threads)" } ?? "(none)")
                + " -> live fds \(live.fds)/threads \(live.threads) -> after teardown "
                + (tornDown.map { "fds \($0.fds)/threads \($0.threads)" } ?? "(none)")
                + " -> after the last engine reference was dropped "
                + (released.map { "fds \($0.fds)/threads \($0.threads)" } ?? "(none)")
        )
        XCTAssertTrue(
            tearDownReturned,
            "the app's teardown never returned: it printed QUITTING but never TORNDOWN within "
                + "\(Self.terminationBudget)s of being told to quit. In '\(mode)' mode that is "
                + "\(mode == "explicit" ? "`shutdown()`" : "`NMPEngine.deinit`") not coming back "
                + "-- a wedged teardown, which is the exact failure a bare "
                + "'the process is not running any more' check would miss."
        )
        XCTAssertTrue(
            exited,
            "the app did not terminate within \(Self.terminationBudget)s of being told to quit "
                + "and had to be SIGKILLed. Nothing below this line is evidence about clean "
                + "shutdown. Tail: \(feed.tail())"
        )
        XCTAssertEqual(
            reason, .exit,
            "the app did not exit of its own accord -- it was terminated by a signal. Termination "
                + "under a signal is what C9 tests; C18's claim is that an app that is ASKED to "
                + "quit does."
        )
        XCTAssertEqual(
            status, 0,
            "the app exited \(status) after \(String(format: "%.2f", elapsed))s. Tail: \(feed.tail())"
        )
        XCTAssertTrue(
            pidGone, "pid \(pid) still exists after the process was reported exited"
        )

        // NOTHING IS LEFT RUNNING, measured while the process is still alive
        // to be measured. This is the assertion that stops C18 from being a
        // tautology: once a process has exited its threads and sockets are
        // gone whatever NMP did, so "the app terminated" alone would pass
        // for an engine that released nothing and was rescued by `exit()`.
        // The reading below is taken AFTER the last engine reference was
        // dropped and BEFORE the process ends -- the only window in which
        // NMP's own teardown is distinguishable from the kernel's.
        XCTAssertTrue(
            reportedRelease, "the app never reported its post-release resource counts"
        )
        // What separates the two modes, asserted only where it is the
        // claim: `shutdown()` must release resources BY ITSELF, before any
        // reference is dropped. Without this the explicit test would be
        // indistinguishable from the implicit one -- both would pass for an
        // engine whose `shutdown()` did nothing at all and whose `deinit`
        // did the work. Measured: 14 fds/19 threads live -> 6/5 the instant
        // `shutdown()` returned.
        if mode == "explicit", let tornDown {
            XCTAssertLessThan(
                tornDown.fds, live.fds,
                "`shutdown()` returned with \(tornDown.fds) file descriptors still open against "
                    + "\(live.fds) while live -- it released nothing of its own, and everything "
                    + "this mode claims over the implicit one is unproven."
            )
            XCTAssertLessThan(
                tornDown.threads, live.threads,
                "`shutdown()` returned with \(tornDown.threads) live threads against "
                    + "\(live.threads) while live -- the engine thread it is supposed to stop is "
                    + "still there."
            )
        }
        // Compared against the app's OWN pre-engine baseline, not against
        // its live counts. That distinction is a falsification result
        // rather than a preference: the first draft asserted only
        // `released < live`, and deliberately leaking the engine into a
        // global still PASSED it -- releasing the query and diagnostics
        // handles alone moved 14 fds/19 threads down to 11/17, which is
        // strictly less than live and nowhere near released. A baseline
        // cannot be half-satisfied.
        if let released, let baseline {
            XCTAssertLessThanOrEqual(
                released.fds, baseline.fds,
                "in '\(mode)' mode the app held \(released.fds) file descriptors after the last "
                    + "engine reference was dropped, against \(baseline.fds) before the engine "
                    + "existed and \(live.fds) while it was live. Quitting did not return the "
                    + "process to where it started -- \(released.fds - baseline.fds) descriptor(s) "
                    + "the engine opened are still open."
            )
            XCTAssertLessThanOrEqual(
                released.threads, baseline.threads,
                "in '\(mode)' mode the app had \(released.threads) live threads after the last "
                    + "engine reference was dropped, against \(baseline.threads) before the "
                    + "engine existed and \(live.threads) while it was live. In 'implicit' mode "
                    + "this is exactly the claim `NMPEngine.deinit` makes -- \"an app that "
                    + "forgets to call this explicitly does not leak the engine thread\" -- and "
                    + "nothing else tests it."
            )
        } else {
            XCTFail(
                "the app did not report both a pre-engine baseline and a post-release reading, "
                    + "so there is nothing to compare: \(feed.tail())"
            )
        }

        // --- Phase 3: nothing is left running ------------------------------
        //
        // With the process gone, its threads and sockets are gone by
        // construction, so asserting them again from outside would be
        // theatre. The two facts that are NOT implied by exit, and are
        // asserted here, are that the relay's port is still perfectly
        // healthy (so a dead relay cannot be silently doing the work of a
        // clean client shutdown) and that NMP's own cross-process store
        // lock was RELEASED rather than left stale -- the same call that
        // refused above must now succeed.

        let relayStillHealthy = await relay.isReachable(timeout: 5)
        XCTAssertTrue(
            relayStillHealthy,
            "the relay stopped accepting connections during this scenario, so 'the app shut down "
                + "cleanly' cannot be distinguished from 'the app's peer disappeared'."
        )

        // --- Phase 4: nothing durable was left HALF-WRITTEN ----------------
        //
        // The relay is killed FIRST and required to refuse a real TCP
        // connection, so every row read below came off disk and nothing
        // could have been re-fetched (C2's own technique). A fresh engine
        // then opens the store the dead app owned.

        try await relay.kill()
        let relayDead = await waitUntil(timeout: 10) { await !relay.isReachable() }
        XCTAssertTrue(
            relayDead,
            "PRECONDITION for the durability check: \(relay.url) still accepts a real TCP "
                + "connection after the relay was SIGKILLed, so a row read below could have come "
                + "from the network rather than the store."
        )

        var reopened: NMPEngine?
        var reopenError = "none"
        do {
            reopened = try NMPEngine(config: NMPConfig(storePath: storePath, appRelays: [relay.url]))
        } catch {
            reopenError = "\(error)"
        }
        note("reopen: succeeded=\(reopened != nil) error=\(reopenError)")
        guard let engine = reopened else {
            return XCTFail(
                "a fresh NMPEngine could not open the store the quit app left behind: "
                    + "\(reopenError). Either the store is half-written or #489's ownership lock "
                    + "was never released, and both are the same product failure to an app that "
                    + "cannot start twice."
            )
        }
        defer { engine.shutdown() }

        // The app's own read state: the relay-served row is still there.
        let seededID = seeded.id
        let seenRows = await firstMatchingBatch(
            engine: engine,
            filter: NMPFilter(kinds: [1], authors: .literal([keyPair.pubkeyHex])),
            timeout: 20
        ) { $0.rows.contains { $0.id == seededID } }
        // The app's own WRITE state: the event it published and signed.
        let ownEventID = live.signedEventID
        let ownRows = await firstMatchingBatch(
            engine: engine,
            filter: NMPFilter(kinds: [1], authors: .literal([live.accountHex])),
            timeout: 20
        ) { $0.rows.contains { $0.id == ownEventID } }

        note(
            "durable: seededRowBack=\(seenRows?.rows.count ?? -1) rows, "
                + "ownWriteBack=\(ownRows?.rows.count ?? -1) rows (id \(live.signedEventID))"
        )
        XCTAssertNotNil(
            seenRows,
            "the row the relay served the quit app is not readable from the store it left "
                + "behind, with the relay provably dead. Its read state did not survive the quit."
        )
        XCTAssertNotNil(
            ownRows,
            "the app's own signed, published event \(live.signedEventID) is not readable from "
                + "the store it left behind. Its write state did not survive the quit -- the "
                + "'nothing durable left half-written' half of C18."
        )
        // The session, RECORDED rather than asserted, and 0 is the correct
        // answer: an account lives in the exported `NMPSessionPayload`, not
        // in the store, and this app deliberately never exported one. C2
        // already proves the restore path. Printed so that a future change
        // making accounts store-resident shows up here as a changed number
        // rather than silently.
        let accounts = (try? engine.session.accounts) ?? []
        note("durable: accounts=\(accounts.count) (0 is correct -- no session payload was exported)")
    }

    /// How long the app gets, from being told to quit, to be gone. Deliberately
    /// generous: this is not a performance budget, it is the difference
    /// between terminating and hanging, and a bound that fails on a busy
    /// machine would be a coin flip rather than an oracle (C17's lesson
    /// about instrument resolution, applied to a clock instead of a byte
    /// count).
    private static let terminationBudget: TimeInterval = 30

    // MARK: - Reading the child

    private struct LiveLine {
        let wireSubs: Int
        let rows: Int
        let rowsSourced: Int
        let signedEventID: String
        let fds: Int
        let threads: Int
        let accountHex: String
    }

    private struct TornDownLine {
        let fds: Int
        let threads: Int
    }

    private struct ChildSnapshot {
        var baseline: TornDownLine?
        var live: LiveLine?
        var quitting: String?
        var tornDown: TornDownLine?
        var released: TornDownLine?
    }

    private final class ChildFeed: @unchecked Sendable {
        private let lock = NSLock()
        private var pending = Data()
        private var state = ChildSnapshot()
        private var lines: [String] = []

        func ingest(_ data: Data) {
            guard !data.isEmpty else { return }
            lock.lock()
            pending.append(data)
            while let index = pending.firstIndex(of: UInt8(ascii: "\n")) {
                let lineData = pending[pending.startIndex..<index]
                pending = pending[pending.index(after: index)...]
                if let line = String(data: Data(lineData), encoding: .utf8) { apply(line) }
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
            return lines.suffix(10).joined(separator: " | ")
        }

        /// Caller holds the lock.
        private func apply(_ line: String) {
            lines.append(line)
            if lines.count > 200 { lines.removeFirst(100) }
            if line.hasPrefix("BASELINE:") {
                let f = line.dropFirst("BASELINE:".count).split(separator: ",").map(String.init)
                guard f.count == 2, let fds = Int(f[0]), let threads = Int(f[1]) else { return }
                state.baseline = TornDownLine(fds: fds, threads: threads)
            } else if line.hasPrefix("LIVE:") {
                let f = line.dropFirst("LIVE:".count).split(separator: ",").map(String.init)
                guard f.count == 7, let subs = Int(f[0]), let rows = Int(f[1]),
                    let sourced = Int(f[2]), let fds = Int(f[4]), let threads = Int(f[5])
                else { return }
                state.live = LiveLine(
                    wireSubs: subs, rows: rows, rowsSourced: sourced, signedEventID: f[3],
                    fds: fds, threads: threads, accountHex: f[6]
                )
            } else if line.hasPrefix("QUITTING:") {
                state.quitting = String(line.dropFirst("QUITTING:".count))
            } else if line.hasPrefix("TORNDOWN:") {
                let f = line.dropFirst("TORNDOWN:".count).split(separator: ",").map(String.init)
                guard f.count == 2, let fds = Int(f[0]), let threads = Int(f[1]) else { return }
                state.tornDown = TornDownLine(fds: fds, threads: threads)
            } else if line.hasPrefix("RELEASED:") {
                let f = line.dropFirst("RELEASED:".count).split(separator: ",").map(String.init)
                guard f.count == 2, let fds = Int(f[0]), let threads = Int(f[1]) else { return }
                state.released = TornDownLine(fds: fds, threads: threads)
            }
        }
    }

    // MARK: - External observation

    /// How many ESTABLISHED TCP connections `pid` holds to `port`, per
    /// `lsof`. Deliberately an OUTSIDE observer: an app claiming through
    /// `observeDiagnostics()` that it is connected is NMP's own account of
    /// itself, and C13 already recorded that `wireSubCount` counts planned
    /// subscriptions rather than live sockets. This is the kernel's answer.
    /// Returns -1 if `lsof` could not be run at all, which fails the
    /// assertion rather than skipping it.
    private static func establishedConnections(pid: Int32, port: UInt16) -> Int {
        let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"]
            .first { FileManager.default.isExecutableFile(atPath: $0) }
        guard let lsof else { return -1 }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: lsof)
        process.arguments = ["-a", "-p", String(pid), "-i", "-n", "-P"]
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
        } catch {
            return -1
        }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        let text = String(data: data, encoding: .utf8) ?? ""
        return text.split(separator: "\n").filter {
            $0.contains("ESTABLISHED") && $0.contains(":\(port)")
        }.count
    }

    /// The first delivered batch satisfying `predicate`, or `nil` within the
    /// bound. Bounded by construction; the caller reports the real result.
    private func firstMatchingBatch(
        engine: NMPEngine,
        filter: NMPFilter,
        timeout: TimeInterval,
        _ predicate: @escaping @Sendable (RowBatch) -> Bool
    ) async -> RowBatch? {
        guard let query = try? engine.observe(.single(NMPDemand(selection: filter))) else { return nil }
        defer { query.cancel() }
        return await withTaskGroup(of: RowBatch?.self) { group in
            group.addTask {
                do {
                    for try await batch in query where predicate(batch) { return batch }
                } catch {}
                return nil
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: UInt64(timeout * 1_000_000_000))
                return nil
            }
            let result = await group.next() ?? nil
            group.cancelAll()
            return result
        }
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

    // MARK: - Locating the two prerequisites

    private static func quitterBinaryURL() throws -> URL {
        let productsDir = Bundle(for: C18CleanShutdownTests.self)
            .bundleURL.deletingLastPathComponent()
        let candidate = productsDir.appendingPathComponent("canary-c18-quitter")
        guard FileManager.default.isExecutableFile(atPath: candidate.path) else {
            throw XCTSkip("canary-c18-quitter not found next to the test bundle at \(candidate.path)")
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
