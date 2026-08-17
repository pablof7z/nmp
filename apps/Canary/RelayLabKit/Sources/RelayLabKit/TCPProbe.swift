// TCPProbe.swift
//
// A TCP connect probe that reports WHY it failed, not merely that it did.
//
// `RelayHandle.isReachable` answers a boolean and is the right tool for
// "wait until this port stops answering". It cannot answer the question C8
// has to ask, and measurement rather than reading is how that was
// established: probing a SIGKILLed relay's loopback port with
// `isReachable(timeout: 2)` returned `false` after 2.0057 seconds -- the
// full budget, not a refusal. The cause is `Network.framework`'s own
// design: a connection to a port with nothing behind it enters
// `NWConnection.State.waiting(.posix(ECONNREFUSED))` and keeps retrying,
// because `NWConnection` is built for connections that may become viable.
// It never reaches `.failed`, so a probe that watches for `.failed` can
// only ever end on its own timeout. A refused port and a black-holed one
// are therefore indistinguishable through that door, and both cost the
// caller the whole timeout.
//
// "The relay is DOWN" and "the relay is slow, filtered, or hung" are
// different failures that a write path handles differently -- one fails on
// the first syscall, the other on an ack timeout -- so a scenario claiming
// the first has to observe `ECONNREFUSED` itself. That means a plain BSD
// socket, where the errno is the answer rather than something inferred
// from a state machine's silence.
//
// Additive file rather than a change to `RelayHandle`: the Canary's relay
// lab is being extended by several scenarios at once, and this is the
// shape that cannot conflict with any of them.

import Foundation
#if canImport(Darwin)
import Darwin
#endif

/// What one real TCP connect attempt actually did.
public enum TCPProbeOutcome: Sendable, Equatable {
    /// The port accepted a connection.
    case accepted
    /// The kernel got an RST: there is nothing listening, and it said so
    /// immediately. This -- and only this -- is "the relay is down".
    case refused
    /// The connect failed for some other reason, carrying the errno so the
    /// caller reports a real value rather than a guess.
    case failed(errno: Int32)
    /// Nothing answered within the budget: filtered, black-holed, or a
    /// listener that accepted the SYN and then stalled. NOT a refusal.
    case timedOut
}

public struct TCPProbe: Sendable, Equatable {
    public let outcome: TCPProbeOutcome
    /// How long the attempt took. A refusal on loopback is sub-millisecond;
    /// a timeout consumes the whole budget. Printing this is what lets a
    /// scenario show, rather than assert, which one it got.
    public let elapsed: TimeInterval
}

public extension RelayHandle {
    /// One real TCP connect to this relay's port, reporting the actual
    /// outcome and how long it took.
    func probe(timeout: TimeInterval = 2) async -> TCPProbe {
        await TCPProbe.connect(host: "127.0.0.1", port: port, timeout: timeout)
    }
}

public extension TCPProbe {
    /// Non-blocking `connect(2)` + `poll(2)` + `SO_ERROR`. Deliberately the
    /// raw syscalls: the errno IS the fact being reported, and every higher
    /// level API in this direction erases it.
    static func connect(
        host: String, port: UInt16, timeout: TimeInterval
    ) async -> TCPProbe {
        await withCheckedContinuation { (continuation: CheckedContinuation<TCPProbe, Never>) in
            DispatchQueue.global().async {
                continuation.resume(returning: connectBlocking(host: host, port: port, timeout: timeout))
            }
        }
    }

    private static func connectBlocking(
        host: String, port: UInt16, timeout: TimeInterval
    ) -> TCPProbe {
        let started = Date()
        func done(_ outcome: TCPProbeOutcome) -> TCPProbe {
            TCPProbe(outcome: outcome, elapsed: Date().timeIntervalSince(started))
        }

        let sock = socket(AF_INET, SOCK_STREAM, 0)
        guard sock >= 0 else { return done(.failed(errno: errno)) }
        defer { Darwin.close(sock) }

        let flags = fcntl(sock, F_GETFL, 0)
        guard flags >= 0, fcntl(sock, F_SETFL, flags | O_NONBLOCK) >= 0 else {
            return done(.failed(errno: errno))
        }

        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        addr.sin_addr.s_addr = inet_addr(host)

        let connectResult = withUnsafePointer(to: &addr) { ptr -> Int32 in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockaddrPtr in
                Darwin.connect(sock, sockaddrPtr, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        if connectResult == 0 { return done(.accepted) }
        let immediate = errno
        if immediate != EINPROGRESS {
            return done(immediate == ECONNREFUSED ? .refused : .failed(errno: immediate))
        }

        var fds = pollfd(fd: sock, events: Int16(POLLOUT), revents: 0)
        let pollResult = withUnsafeMutablePointer(to: &fds) { ptr in
            poll(ptr, 1, Int32(timeout * 1000))
        }
        if pollResult == 0 { return done(.timedOut) }
        if pollResult < 0 { return done(.failed(errno: errno)) }

        var soError: Int32 = 0
        var length = socklen_t(MemoryLayout<Int32>.size)
        guard getsockopt(sock, SOL_SOCKET, SO_ERROR, &soError, &length) == 0 else {
            return done(.failed(errno: errno))
        }
        if soError == 0 { return done(.accepted) }
        return done(soError == ECONNREFUSED ? .refused : .failed(errno: soError))
    }
}
