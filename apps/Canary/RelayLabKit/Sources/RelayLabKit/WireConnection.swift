// WireConnection.swift
//
// A thin, real WebSocket client (URLSessionWebSocketTask) over the real
// Nostr wire protocol. Used by the lab CONTROLLER only -- to seed relays
// and to drive the NIP-42 client handshake -- never by the relay under
// test. Every read is bounded by an explicit timeout; nothing here can
// hang a scenario indefinitely.

import Foundation

public enum WireError: Error, CustomStringConvertible {
    case timedOut(String)
    case notConnected

    public var description: String {
        switch self {
        case .timedOut(let what): return "timed out waiting for \(what)"
        case .notConnected: return "connection is not open"
        }
    }
}

/// One real WebSocket connection to a relay. Each instance is ONE
/// connection -- deliberately not reused across "fresh connection" tests,
/// since the whole point there is a NEW TCP/WS handshake.
public final class WireConnection: NSObject {
    private let task: URLSessionWebSocketTask
    private let session: URLSession

    public init(url: URL) {
        session = URLSession(configuration: .ephemeral)
        task = session.webSocketTask(with: url)
        super.init()
        task.resume()
    }

    /// Bounded send -- discovered necessary while testing `partition()`
    /// (SIGSTOP): an unbounded `task.send` can block indefinitely once the
    /// peer stops reading (the kernel socket write buffer fills and
    /// URLSessionWebSocketTask's completion never fires). Same
    /// unstructured-task race pattern as `receiveLine`, for the same
    /// reason -- a task-group `cancelAll()` alone does not reliably
    /// unblock an in-flight URLSessionWebSocketTask operation.
    public func send(_ text: String, timeout: TimeInterval = 5) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            let guardOnce = ResumeOnce()
            let sendTask = Task { [task] in
                do {
                    try await task.send(.string(text))
                    guard guardOnce.claim() else { return }
                    continuation.resume(returning: ())
                } catch {
                    guard guardOnce.claim() else { return }
                    continuation.resume(throwing: error)
                }
            }
            Task {
                try? await Task.sleep(nanoseconds: UInt64(max(0, timeout) * 1_000_000_000))
                guard guardOnce.claim() else { return }
                sendTask.cancel()
                continuation.resume(throwing: WireError.timedOut("send of \(text.prefix(40))..."))
            }
        }
    }

    /// Reads exactly one text frame, bounded by `timeout`. Throws
    /// `WireError.timedOut` rather than hanging if nothing arrives -- a
    /// scenario that races an indefinite wait is not trustworthy evidence.
    ///
    /// NOT implemented as a `withThrowingTaskGroup` race between
    /// `task.receive()` and a `Task.sleep` loser: that shape was tried
    /// first and genuinely hung, because exiting a task-group scope
    /// implicitly awaits every child task to completion (even ones
    /// `cancelAll()`-cancelled), and `URLSessionWebSocketTask.receive()`
    /// does not appear to unblock on structured-concurrency cancellation
    /// alone -- it kept waiting on the real socket forever with nothing
    /// left to ever wake it. Using two UNSTRUCTURED tasks racing into one
    /// continuation avoids that: on timeout we resume immediately and
    /// merely request best-effort cancellation of the loser, we never wait
    /// for it.
    public func receiveLine(timeout: TimeInterval, what: String = "a frame") async throws -> String {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<String, Error>) in
            let guardOnce = ResumeOnce()

            let receiveTask = Task { [task] in
                do {
                    let message = try await task.receive()
                    guard guardOnce.claim() else { return }
                    switch message {
                    case .string(let s):
                        continuation.resume(returning: s)
                    case .data(let d):
                        if let s = String(data: d, encoding: .utf8) {
                            continuation.resume(returning: s)
                        } else {
                            continuation.resume(throwing: WireError.timedOut(what))
                        }
                    @unknown default:
                        continuation.resume(throwing: WireError.timedOut(what))
                    }
                } catch {
                    guard guardOnce.claim() else { return }
                    continuation.resume(throwing: error)
                }
            }

            Task {
                try? await Task.sleep(nanoseconds: UInt64(max(0, timeout) * 1_000_000_000))
                guard guardOnce.claim() else { return }
                receiveTask.cancel()
                continuation.resume(throwing: WireError.timedOut(what))
            }
        }
    }

    public func close() {
        task.cancel(with: .goingAway, reason: nil)
    }
}

/// Parsed shape of the two NIP-01/NIP-42 relay->client messages the lab
/// controller cares about. Anything else observed on the wire is ignored
/// (not every relay message is relevant to a seed/auth probe).
public enum RelayFrame {
    case ok(eventId: String, accepted: Bool, message: String)
    case authChallenge(String)
    case other(String)

    public static func parse(_ line: String) -> RelayFrame {
        guard let data = line.data(using: .utf8),
              let arr = try? JSONSerialization.jsonObject(with: data) as? [Any],
              let tag = arr.first as? String
        else { return .other(line) }

        switch tag {
        case "OK" where arr.count >= 3:
            let id = arr[1] as? String ?? ""
            let accepted = arr[2] as? Bool ?? false
            let msg = (arr.count >= 4 ? arr[3] as? String : nil) ?? ""
            return .ok(eventId: id, accepted: accepted, message: msg)
        case "AUTH" where arr.count >= 2:
            let challenge = arr[1] as? String ?? ""
            return .authChallenge(challenge)
        default:
            return .other(line)
        }
    }
}
