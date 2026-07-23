#if targetEnvironment(simulator)
import CryptoKit
import Foundation
@preconcurrency import Network
@testable import NMP

/// Test-owned hostname relay for full public-API read/write qualification on
/// a real iOS Simulator. Ported from the original issue #598 reproducer in
/// Pod0 so the upstream regression proof exercises the same wire boundary.
final class ControlledRelayHarness: @unchecked Sendable {
    struct Snapshot: Sendable {
        let nip11Requests: Int
        let requestSubscriptionIDs: [String]
        let closedSubscriptionIDs: [String]
        let acceptedEventIDs: [String]
        let activeWebSockets: Int
        let peakActiveWebSockets: Int
    }

    private struct Frame {
        let opcode: UInt8
        let payload: Data
    }

    private struct SnapshotWaiter {
        let predicate: @Sendable (Snapshot) -> Bool
        let continuation: CheckedContinuation<Snapshot?, Never>
    }

    private enum WaitRegistration {
        case stored
        case resume(Snapshot?)
    }

    private enum HarnessError: Error {
        case listenerDidNotStart
    }

    private let listener: NWListener
    private let queue = DispatchQueue(label: "nmp.simulator.controlled-relay")
    private let lock = NSLock()
    private var webSockets: [ObjectIdentifier: NWConnection] = [:]
    private var subscriptions: [String: NWConnection] = [:]
    private var seededEvents: [[String: Any]] = []
    private var nip11RequestCount = 0
    private var requestIDs: [String] = []
    private var closeIDs: [String] = []
    private var eventIDs: [String] = []
    private var peakWebSockets = 0
    private var snapshotWaiters: [UUID: SnapshotWaiter] = [:]

    private(set) var relayURL = ""

    init() throws {
        listener = try NWListener(using: .tcp, on: .any)
        let ready = DispatchSemaphore(value: 0)
        listener.stateUpdateHandler = { state in
            if case .ready = state {
                ready.signal()
            }
        }
        listener.newConnectionHandler = { [weak self] connection in
            self?.accept(connection)
        }
        listener.start(queue: queue)
        guard ready.wait(timeout: .now() + 3) == .success, let port = listener.port else {
            listener.cancel()
            throw HarnessError.listenerDidNotStart
        }
        relayURL = "ws://localhost:\(port.rawValue)"
    }

    func seed(_ event: NMPSignedEvent) {
        mutate {
            seededEvents.append([
                "id": event.id,
                "pubkey": event.pubkey,
                "created_at": event.createdAt,
                "kind": event.kind,
                "tags": event.tags,
                "content": event.content,
                "sig": event.signature,
            ])
        }
    }

    func snapshot() -> Snapshot {
        lock.withLock { makeSnapshotLocked() }
    }

    /// Suspend on one exact harness mutation. Cancellation (including the
    /// bounded timeout race in the test) removes and resumes only this
    /// registration, so a timed-out waiter cannot leak into a later edge.
    func nextSnapshot(
        matching predicate: @escaping @Sendable (Snapshot) -> Bool
    ) async -> Snapshot? {
        let id = UUID()
        return await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                let registration = lock.withLock { () -> WaitRegistration in
                    if Task.isCancelled {
                        return .resume(nil)
                    }
                    let snapshot = makeSnapshotLocked()
                    if predicate(snapshot) {
                        return .resume(snapshot)
                    }
                    snapshotWaiters[id] = SnapshotWaiter(
                        predicate: predicate,
                        continuation: continuation
                    )
                    return .stored
                }
                if case .resume(let snapshot) = registration {
                    continuation.resume(returning: snapshot)
                }
            }
        } onCancel: {
            self.cancelSnapshotWaiter(id)
        }
    }

    func snapshotWaiterCount() -> Int {
        lock.withLock { snapshotWaiters.count }
    }

    func stop() {
        listener.cancel()
        let connections = lock.withLock { Array(webSockets.values) }
        connections.forEach { $0.cancel() }
    }

    private func accept(_ connection: NWConnection) {
        connection.stateUpdateHandler = { [weak self, weak connection] state in
            guard let self, let connection else { return }
            if case .failed = state {
                self.connectionEnded(connection)
            }
            if case .cancelled = state {
                self.connectionEnded(connection)
            }
        }
        connection.start(queue: queue)
        receiveHTTPHeaders(on: connection, buffered: Data())
    }

    private func receiveHTTPHeaders(on connection: NWConnection, buffered: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, complete, error in
            guard let self else { return }
            var received = buffered
            if let data {
                received.append(data)
            }
            guard let boundary = received.range(of: Data("\r\n\r\n".utf8)) else {
                if error == nil, !complete {
                    self.receiveHTTPHeaders(on: connection, buffered: received)
                } else {
                    connection.cancel()
                }
                return
            }

            let headers = String(decoding: received[..<boundary.upperBound], as: UTF8.self)
            let remainder = Data(received[boundary.upperBound...])
            if headers.lowercased().contains("upgrade: websocket") {
                self.upgrade(connection, headers: headers, remainder: remainder)
            } else {
                self.sendRelayInformation(on: connection)
            }
        }
    }

    private func sendRelayInformation(on connection: NWConnection) {
        let body = Data(
            #"{"name":"NMP Simulator Relay","supported_nips":[1,11],"software":"nmp-test-harness","version":"1"}"#.utf8
        )
        let headers = Data(
            ("HTTP/1.1 200 OK\r\n" +
                "Content-Type: application/nostr+json\r\n" +
                "Cache-Control: max-age=60\r\n" +
                "Content-Length: \(body.count)\r\n" +
                "Connection: close\r\n\r\n").utf8
        )
        mutate {
            nip11RequestCount += 1
        }
        connection.send(content: headers + body, completion: .contentProcessed { [weak self] _ in
            self?.awaitClientCloseThenCancel(connection)
        })
    }

    private func awaitClientCloseThenCancel(_ connection: NWConnection) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 1) {
            [weak self] _, _, isComplete, error in
            if isComplete || error != nil {
                connection.cancel()
            } else {
                self?.awaitClientCloseThenCancel(connection)
            }
        }
    }

    private func upgrade(_ connection: NWConnection, headers: String, remainder: Data) {
        guard let key = header(named: "sec-websocket-key", in: headers) else {
            connection.cancel()
            return
        }
        let acceptSeed = Data((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").utf8)
        let accept = Data(Insecure.SHA1.hash(data: acceptSeed)).base64EncodedString()
        let response = Data(
            ("HTTP/1.1 101 Switching Protocols\r\n" +
                "Upgrade: websocket\r\n" +
                "Connection: Upgrade\r\n" +
                "Sec-WebSocket-Accept: \(accept)\r\n\r\n").utf8
        )
        mutate {
            webSockets[ObjectIdentifier(connection)] = connection
            peakWebSockets = max(peakWebSockets, webSockets.count)
        }
        connection.send(content: response, completion: .contentProcessed { [weak self] error in
            guard let self else { return }
            if error == nil {
                self.receiveFrames(on: connection, buffered: remainder)
            } else {
                connection.cancel()
            }
        })
    }

    private func receiveFrames(on connection: NWConnection, buffered: Data) {
        var buffered = buffered
        while let (frame, consumed) = parseFrame(buffered) {
            buffered.removeFirst(consumed)
            handle(frame, from: connection)
        }
        let pending = buffered
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, complete, error in
            guard let self else { return }
            var next = pending
            if let data {
                next.append(data)
            }
            if error == nil, !complete {
                self.receiveFrames(on: connection, buffered: next)
            } else {
                connection.cancel()
            }
        }
    }

    private func handle(_ frame: Frame, from connection: NWConnection) {
        switch frame.opcode {
        case 0x1:
            handleText(frame.payload, from: connection)
        case 0x8:
            sendFrame(opcode: 0x8, payload: frame.payload, on: connection)
            connection.cancel()
        case 0x9:
            sendFrame(opcode: 0xA, payload: frame.payload, on: connection)
        default:
            break
        }
    }

    private func handleText(_ data: Data, from connection: NWConnection) {
        guard let message = try? JSONSerialization.jsonObject(with: data) as? [Any],
              let command = message.first as? String else {
            return
        }
        switch command {
        case "REQ":
            guard message.count >= 3, let subscriptionID = message[1] as? String else {
                return
            }
            let events = mutate {
                subscriptions[subscriptionID] = connection
                requestIDs.append(subscriptionID)
                return seededEvents
            }
            let filters = message.dropFirst(2).compactMap { $0 as? [String: Any] }
            events
                .filter { event in
                    filters.contains { Self.event(event, matches: $0) }
                }
                .forEach {
                    sendJSON(["EVENT", subscriptionID, $0], on: connection)
                }
            sendJSON(["EOSE", subscriptionID], on: connection)
        case "CLOSE":
            guard message.count >= 2, let subscriptionID = message[1] as? String else {
                return
            }
            mutate {
                subscriptions.removeValue(forKey: subscriptionID)
                closeIDs.append(subscriptionID)
            }
        case "EVENT":
            guard message.count >= 2,
                  let event = message[1] as? [String: Any],
                  let eventID = event["id"] as? String else {
                return
            }
            let subscribers = mutate { () -> [(String, NWConnection)] in
                eventIDs.append(eventID)
                seededEvents.append(event)
                return Array(subscriptions)
            }
            sendJSON(["OK", eventID, true, "accepted by NMP simulator relay"], on: connection)
            subscribers.forEach { subscriptionID, subscriber in
                sendJSON(["EVENT", subscriptionID, event], on: subscriber)
            }
        default:
            break
        }
    }

    private static func event(_ event: [String: Any], matches filter: [String: Any]) -> Bool {
        if let kinds = filter["kinds"] as? [Int],
           let kind = event["kind"] as? Int,
           !kinds.contains(kind) {
            return false
        }
        if let authors = filter["authors"] as? [String],
           let pubkey = event["pubkey"] as? String,
           !authors.contains(where: { pubkey.hasPrefix($0) }) {
            return false
        }
        return true
    }

    private func connectionEnded(_ connection: NWConnection) {
        mutate {
            webSockets.removeValue(forKey: ObjectIdentifier(connection))
            subscriptions = subscriptions.filter { $0.value !== connection }
        }
    }

    private func makeSnapshotLocked() -> Snapshot {
        Snapshot(
            nip11Requests: nip11RequestCount,
            requestSubscriptionIDs: requestIDs,
            closedSubscriptionIDs: closeIDs,
            acceptedEventIDs: eventIDs,
            activeWebSockets: webSockets.count,
            peakActiveWebSockets: peakWebSockets
        )
    }

    /// Apply one state transition and resume every exact waiter whose
    /// predicate becomes true from that transition. Continuations are
    /// removed under the same lock and resumed after unlocking.
    @discardableResult
    private func mutate<Value>(_ body: () -> Value) -> Value {
        let (result, snapshot, matched) = lock.withLock {
            let result = body()
            let snapshot = makeSnapshotLocked()
            let matchingIDs = snapshotWaiters.compactMap { id, waiter in
                waiter.predicate(snapshot) ? id : nil
            }
            let matched = matchingIDs.compactMap {
                snapshotWaiters.removeValue(forKey: $0)
            }
            return (result, snapshot, matched)
        }
        matched.forEach {
            $0.continuation.resume(returning: snapshot)
        }
        return result
    }

    private func cancelSnapshotWaiter(_ id: UUID) {
        let waiter = lock.withLock {
            snapshotWaiters.removeValue(forKey: id)
        }
        waiter?.continuation.resume(returning: nil)
    }

    private func sendJSON(_ object: [Any], on connection: NWConnection) {
        guard let data = try? JSONSerialization.data(withJSONObject: object) else {
            return
        }
        sendFrame(opcode: 0x1, payload: data, on: connection)
    }

    private func sendFrame(opcode: UInt8, payload: Data, on connection: NWConnection) {
        var data = Data([0x80 | opcode])
        if payload.count < 126 {
            data.append(UInt8(payload.count))
        } else if payload.count <= Int(UInt16.max) {
            data.append(126)
            var length = UInt16(payload.count).bigEndian
            withUnsafeBytes(of: &length) {
                data.append(contentsOf: $0)
            }
        } else {
            data.append(127)
            var length = UInt64(payload.count).bigEndian
            withUnsafeBytes(of: &length) {
                data.append(contentsOf: $0)
            }
        }
        data.append(payload)
        connection.send(content: data, completion: .contentProcessed { _ in })
    }

    private func parseFrame(_ data: Data) -> (Frame, Int)? {
        guard data.count >= 2 else {
            return nil
        }
        let bytes = [UInt8](data)
        let opcode = bytes[0] & 0x0F
        let masked = bytes[1] & 0x80 != 0
        var length = UInt64(bytes[1] & 0x7F)
        var index = 2
        if length == 126 {
            guard bytes.count >= 4 else {
                return nil
            }
            length = UInt64(bytes[2]) << 8 | UInt64(bytes[3])
            index = 4
        } else if length == 127 {
            guard bytes.count >= 10 else {
                return nil
            }
            length = bytes[2..<10].reduce(0) { ($0 << 8) | UInt64($1) }
            index = 10
        }
        let maskLength = masked ? 4 : 0
        guard length <= UInt64(Int.max),
              bytes.count >= index + maskLength + Int(length) else {
            return nil
        }
        let mask = masked ? Array(bytes[index..<(index + 4)]) : []
        index += maskLength
        var payload = Array(bytes[index..<(index + Int(length))])
        if masked {
            for offset in payload.indices {
                payload[offset] ^= mask[offset % 4]
            }
        }
        return (Frame(opcode: opcode, payload: Data(payload)), index + Int(length))
    }

    private func header(named name: String, in headers: String) -> String? {
        headers.components(separatedBy: "\r\n").compactMap { line -> String? in
            let parts = line.split(separator: ":", maxSplits: 1)
            guard parts.count == 2, parts[0].lowercased() == name else {
                return nil
            }
            return parts[1].trimmingCharacters(in: .whitespaces)
        }.first
    }
}
#endif
