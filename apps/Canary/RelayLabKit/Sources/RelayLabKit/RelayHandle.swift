// RelayHandle.swift
//
// The Canary relay-lab controller's core type: one real relay child
// process (strfry), on an ephemeral port, with an isolated temp data
// directory. Mirrors the Python prototype's proven interface 1:1
// (start/stop/kill/restart/partition/heal/seed) -- see
// scratchpad/canary-lab/lab.py, the artifact that first proved this
// lifecycle against a real strfry process. This Swift port has itself
// run the full start/seed/query/kill/restart/persistence-check/
// partition/heal sequence against a real strfry child process from this
// exact location (`swift run relay-lab-lifecycle <strfry-binary>`) --
// see docs/internals/canary.md.

import Foundation
import Network
#if canImport(Darwin)
import Darwin
#endif

public enum RelayLabError: Error, CustomStringConvertible {
    case alreadyStarted
    case notStarted
    case processExitedDuringStartup(String, Int32)
    case neverAccepted(String, TimeInterval)
    case portAllocationFailed

    public var description: String {
        switch self {
        case .alreadyStarted: return "relay handle already started"
        case .notStarted: return "relay handle is not running"
        case .processExitedDuringStartup(let name, let code):
            return "\(name): relay process exited during startup (code=\(code))"
        case .neverAccepted(let name, let timeout):
            return "\(name): relay never accepted a connection within \(timeout)s"
        case .portAllocationFailed: return "could not allocate an ephemeral port"
        }
    }
}

public final class RelayHandle {
    public let name: String
    public let workDir: URL
    public let dataDir: URL
    public let configPath: URL
    public let logPath: URL
    public let port: UInt16
    private let binaryPath: URL
    private var process: Process?

    public var url: String { "ws://127.0.0.1:\(port)" }
    public var isRunning: Bool { process?.isRunning ?? false }

    /// - Parameters:
    ///   - binaryPath: path to the real strfry binary (see setup-strfry.sh).
    ///   - dataDir: `nil` (the default) gives this relay its own `workDir/data`
    ///     LMDB directory. Passing ANOTHER handle's `dataDir` starts a second
    ///     relay process, on its own ephemeral port, over that same durable
    ///     store -- which is how a scenario writes into a relay's database
    ///     during a window when the relay's OWN port is deliberately dead
    ///     (C13's outage). LMDB is a single-writer store: the two processes
    ///     must not run at the same time, and no lab call enforces that.
    public init(
        name: String, workDir: URL, binaryPath: URL, dataDir sharedDataDir: URL? = nil
    ) async throws {
        self.name = name
        self.workDir = workDir
        self.dataDir = sharedDataDir ?? workDir.appendingPathComponent("data", isDirectory: true)
        self.configPath = workDir.appendingPathComponent("strfry.conf")
        self.logPath = workDir.appendingPathComponent("relay.log")
        self.binaryPath = binaryPath
        try FileManager.default.createDirectory(at: dataDir, withIntermediateDirectories: true)
        self.port = try await Self.freeEphemeralPort()
        try Self.writeConfig(to: configPath, dataDir: dataDir, port: port)
    }

    // MARK: - Lifecycle

    /// Spawns the real child process and blocks until it has ACTUALLY
    /// accepted a real TCP connection -- a bounded poll-and-retry loop via
    /// `Network.framework`, never a fixed `sleep(N)`. Fails loudly
    /// (throws) if the relay never becomes reachable or exits during
    /// startup; a scenario must never silently race ahead of a broken
    /// relay.
    public func start(timeout: TimeInterval = 10) async throws {
        guard process == nil else { throw RelayLabError.alreadyStarted }
        let proc = Process()
        proc.executableURL = binaryPath
        proc.arguments = ["--config", configPath.path, "relay"]
        let handle = try openLogHandleForAppending()
        proc.standardOutput = handle
        proc.standardError = handle
        try proc.run()
        process = proc

        do {
            try await Self.waitForRealAccept(
                port: port, timeout: timeout,
                isStillRunning: { [weak proc] in proc?.isRunning ?? false }
            )
        } catch RelayLabError.neverAccepted {
            if !proc.isRunning {
                throw RelayLabError.processExitedDuringStartup(name, proc.terminationStatus)
            }
            throw RelayLabError.neverAccepted(name, timeout)
        }
    }

    /// Same data directory, same port, a BRAND NEW OS process. This is the
    /// actual persistence proof -- nothing about the new process is shared
    /// with the old one except what strfry wrote to `dataDir`.
    public func restart(timeout: TimeInterval = 10) async throws {
        guard process == nil else { throw RelayLabError.alreadyStarted }
        try await start(timeout: timeout)
    }

    /// SIGTERM, then a bounded wait for real exit; escalates to SIGKILL if
    /// the process ignores it. Never blocks forever.
    public func stop(timeout: TimeInterval = 5) async throws {
        try await signalAndWait(SIGTERM, timeout: timeout)
    }

    /// SIGKILL -- simulates a real crashed relay, not a graceful shutdown.
    public func kill(timeout: TimeInterval = 5) async throws {
        try await signalAndWait(SIGKILL, timeout: timeout)
    }

    /// SIGSTOP: freezes the real OS process. Its TCP connections stay
    /// open but nothing answers -- the closest thing to a real network
    /// partition achievable without root (pfctl/firewall rules need sudo,
    /// which the lab cannot assume it has).
    public func partition() {
        guard let proc = process, proc.isRunning else { return }
        Foundation.kill(proc.processIdentifier, SIGSTOP)
    }

    public func heal() {
        guard let proc = process, proc.isRunning else { return }
        Foundation.kill(proc.processIdentifier, SIGCONT)
    }

    /// Attempts ONE real TCP connection to this relay's port and reports
    /// whether it was accepted. `isRunning` only says whether this handle's
    /// child process object is alive; this says whether anything at all is
    /// listening, which is the fact a scenario claiming "the relay is
    /// unreachable" actually needs. A frozen (`partition()`ed) relay still
    /// answers a TCP connect, so this is a liveness probe of the LISTENER,
    /// never a proof that the relay is responsive.
    public func isReachable(timeout: TimeInterval = 1) async -> Bool {
        await Self.attemptConnect(port: port, timeout: timeout)
    }

    private func signalAndWait(_ signal: Int32, timeout: TimeInterval) async throws {
        guard let proc = process else { throw RelayLabError.notStarted }
        Foundation.kill(proc.processIdentifier, signal)
        let deadline = Date().addingTimeInterval(timeout)
        while proc.isRunning, Date() < deadline {
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        if proc.isRunning {
            Foundation.kill(proc.processIdentifier, SIGKILL)
            proc.waitUntilExit()
        }
        process = nil
    }

    // MARK: - Seeding (real wire protocol, never a private DB write)

    /// Publishes one real event over a fresh WebSocket connection and
    /// waits for a real `OK`. Throws if the relay refuses or never
    /// answers within `timeout` -- a seed step that silently "succeeds"
    /// without a real OK is not evidence of anything.
    @discardableResult
    public func seed(_ event: NostrEvent, timeout: TimeInterval = 5) async throws -> Bool {
        guard let wsURL = URL(string: url) else { throw RelayLabError.portAllocationFailed }
        let conn = WireConnection(url: wsURL)
        defer { conn.close() }
        try await conn.send(event.eventFrame())
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            let remaining = deadline.timeIntervalSinceNow
            guard remaining > 0 else { break }
            let line = try await conn.receiveLine(timeout: remaining, what: "OK for \(event.id)")
            if case .ok(let id, let accepted, let message) = RelayFrame.parse(line), id == event.id {
                if !accepted {
                    throw WireError.timedOut("relay refused \(event.id): \(message)")
                }
                return accepted
            }
        }
        throw WireError.timedOut("OK for \(event.id)")
    }

    /// Overwrites the generated config with caller-supplied text (e.g. to
    /// add `auth { enabled = true ... }` + a `writePolicy` plugin for a
    /// NIP-42 scenario). Must be called before `start()`.
    public func overrideConfig(_ text: String) throws {
        precondition(process == nil, "cannot reconfigure a running relay")
        try text.write(to: configPath, atomically: true, encoding: .utf8)
    }

    // MARK: - Setup helpers

    private func openLogHandleForAppending() throws -> FileHandle {
        if !FileManager.default.fileExists(atPath: logPath.path) {
            FileManager.default.createFile(atPath: logPath.path, contents: nil)
        }
        return try FileHandle(forWritingTo: logPath)
    }

    private static func writeConfig(to path: URL, dataDir: URL, port: UInt16) throws {
        let conf = """
        db = "\(dataDir.path)/"
        relay {
            bind = "127.0.0.1"
            port = \(port)
            info {
                name = "canary-lab-relay"
                description = "prototype"
            }
        }
        """
        try conf.write(to: path, atomically: true, encoding: .utf8)
    }

    /// Binds an ephemeral TCP port via a raw BSD socket (bind to port 0 on
    /// loopback, read back the OS-assigned port via `getsockname`, close),
    /// then releases it. Same inherent TOCTOU as any "find a free port,
    /// then hand it to a child process" scheme (the Python prototype has
    /// the identical race) -- acceptable for a local dev lab, not for
    /// anything adversarial. (`Network.framework`'s `NWListener` was tried
    /// first and failed with `EINVAL` binding a wildcard port in this
    /// environment; plain BSD sockets are the more direct tool for this
    /// specific "grab and release a free port" operation anyway.)
    private static func freeEphemeralPort() async throws -> UInt16 {
        let sock = socket(AF_INET, SOCK_STREAM, 0)
        guard sock >= 0 else { throw RelayLabError.portAllocationFailed }
        defer { Darwin.close(sock) }

        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = 0
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")

        let bindResult = withUnsafePointer(to: &addr) { ptr -> Int32 in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockaddrPtr in
                bind(sock, sockaddrPtr, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard bindResult == 0 else { throw RelayLabError.portAllocationFailed }

        var actual = sockaddr_in()
        var len = socklen_t(MemoryLayout<sockaddr_in>.size)
        let getResult = withUnsafeMutablePointer(to: &actual) { ptr -> Int32 in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockaddrPtr in
                getsockname(sock, sockaddrPtr, &len)
            }
        }
        guard getResult == 0 else { throw RelayLabError.portAllocationFailed }
        return UInt16(bigEndian: actual.sin_port)
    }

    /// Bounded poll-and-retry: attempts a REAL TCP connect via
    /// `Network.framework` every ~50ms until either it succeeds, the
    /// process exits, or `timeout` elapses. This is the single most
    /// important property carried over from the proven Python prototype
    /// (`lab.py`'s `_wait_for_real_accept`) -- a sleep-based readiness
    /// oracle is explicitly disqualifying as Canary evidence.
    static func waitForRealAccept(
        port: UInt16, timeout: TimeInterval, isStillRunning: @escaping () -> Bool
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            guard isStillRunning() else {
                throw RelayLabError.neverAccepted("(process exited)", timeout)
            }
            if await attemptConnect(port: port, timeout: 0.2) {
                return
            }
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        throw RelayLabError.neverAccepted("relay", timeout)
    }

    private static func attemptConnect(port: UInt16, timeout: TimeInterval) async -> Bool {
        await withCheckedContinuation { (continuation: CheckedContinuation<Bool, Never>) in
            guard let nwPort = NWEndpoint.Port(rawValue: port) else {
                continuation.resume(returning: false)
                return
            }
            let conn = NWConnection(host: "127.0.0.1", port: nwPort, using: .tcp)
            let guardOnce = ResumeOnce()
            let finish: @Sendable (Bool) -> Void = { ok in
                guard guardOnce.claim() else { return }
                conn.cancel()
                continuation.resume(returning: ok)
            }
            conn.stateUpdateHandler = { state in
                switch state {
                case .ready: finish(true)
                case .failed, .cancelled: finish(false)
                default: break
                }
            }
            conn.start(queue: .global())
            DispatchQueue.global().asyncAfter(deadline: .now() + timeout) {
                finish(false)
            }
        }
    }
}
