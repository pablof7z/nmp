import CryptoKit
import Foundation
@preconcurrency import Network
import XCTest
@testable import NMP

/// A real local TCP server that speaks just enough of BUD-02 `PUT /upload`
/// to answer the engine-authorized upload: it reads the complete request
/// (headers + `Content-Length` body), records exactly what it received, and
/// replies with a descriptor computed from THOSE bytes.
///
/// Deliberately not a mock of the Swift wrapper: the falsifiers below assert
/// on what actually crossed the socket, which is the only place a claim about
/// "the exact bytes" and "NMP owns the BUD-11 header" can be checked.
private final class LocalBlossomServer: @unchecked Sendable {
    private let listener: NWListener
    private let queue = DispatchQueue(label: "nmp.swift.blossom.fixture")
    private let received = DispatchSemaphore(value: 0)
    private let responseGate: DispatchSemaphore?
    private let lock = NSLock()
    private var head = ""
    private var body = Data()
    private let status: String

    private(set) var serverURL = ""

    init(status: String = "201 Created", gated: Bool = false) throws {
        listener = try NWListener(using: .tcp, on: .any)
        self.status = status
        responseGate = gated ? DispatchSemaphore(value: 0) : nil

        let ready = DispatchSemaphore(value: 0)
        listener.stateUpdateHandler = { state in
            if case .ready = state { ready.signal() }
        }
        listener.newConnectionHandler = { [weak self] connection in
            guard let self else { return }
            connection.start(queue: self.queue)
            self.receive(connection, buffer: Data())
        }
        listener.start(queue: queue)
        guard ready.wait(timeout: .now() + 2) == .success, let port = listener.port else {
            listener.cancel()
            throw FixtureError.listenerDidNotStart
        }
        serverURL = "http://127.0.0.1:\(port.rawValue)"
    }

    deinit {
        responseGate?.signal()
        listener.cancel()
    }

    func waitForRequest(timeout: TimeInterval = 5) -> Bool {
        received.wait(timeout: .now() + timeout) == .success
    }

    func releaseResponse() {
        responseGate?.signal()
    }

    func capturedHead() -> String {
        lock.lock()
        defer { lock.unlock() }
        return head
    }

    func capturedBody() -> Data {
        lock.lock()
        defer { lock.unlock() }
        return body
    }

    func headerValue(_ name: String) -> String? {
        capturedHead()
            .components(separatedBy: "\r\n")
            .compactMap { line -> String? in
                guard let separator = line.firstIndex(of: ":") else { return nil }
                let key = String(line[line.startIndex..<separator])
                guard key.lowercased() == name.lowercased() else { return nil }
                return String(line[line.index(after: separator)...])
                    .trimmingCharacters(in: .whitespaces)
            }
            .first
    }

    private func receive(_ connection: NWConnection, buffer: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 1 << 20) {
            [weak self] data, _, _, error in
            guard let self else { return }
            var buffer = buffer
            if let data { buffer.append(data) }
            guard let marker = buffer.range(of: Data("\r\n\r\n".utf8)) else {
                if error == nil { self.receive(connection, buffer: buffer) }
                return
            }
            let headData = buffer[buffer.startIndex..<marker.upperBound]
            let headText = String(decoding: headData, as: UTF8.self)
            let contentLength = headText
                .components(separatedBy: "\r\n")
                .compactMap { line -> Int? in
                    guard let separator = line.firstIndex(of: ":") else { return nil }
                    guard String(line[line.startIndex..<separator]).lowercased()
                        == "content-length" else { return nil }
                    return Int(
                        String(line[line.index(after: separator)...])
                            .trimmingCharacters(in: .whitespaces)
                    )
                }
                .first ?? 0
            let bodyData = buffer[marker.upperBound...]
            guard bodyData.count >= contentLength else {
                if error == nil { self.receive(connection, buffer: buffer) }
                return
            }

            self.lock.lock()
            self.head = headText
            self.body = Data(bodyData.prefix(contentLength))
            let uploaded = self.body
            self.lock.unlock()
            self.received.signal()
            self.responseGate?.wait()

            let digest = SHA256.hash(data: uploaded).map { String(format: "%02x", $0) }.joined()
            let descriptor = Data(
                """
                {"url":"https://cdn.example/\(digest)","sha256":"\(digest)",\
                "size":\(uploaded.count),"type":"application/pdf"}
                """.utf8
            )
            let headers = Data(
                ("HTTP/1.1 \(self.status)\r\n" +
                    "Content-Type: application/json\r\n" +
                    "Content-Length: \(descriptor.count)\r\n" +
                    "Connection: close\r\n\r\n").utf8
            )
            connection.send(
                content: headers + descriptor,
                completion: .contentProcessed { _ in connection.cancel() }
            )
        }
    }

    private enum FixtureError: Error {
        case listenerDidNotStart
    }
}

final class BlossomUploadTests: XCTestCase {
    private let secret = String(repeating: "0", count: 63) + "1"
    private let author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    private func engineWithAccount() throws -> NMPEngine {
        let engine = try NMPEngine(
            config: NMPConfig(allowedLocalRelayHosts: ["127.0.0.1", "localhost"])
        )
        return engine
    }

    /// #971's headline Swift falsifier: the app states four product inputs,
    /// and the EXACT bytes it named reach the wire under a BUD-11 header the
    /// app never saw, signed by the active account.
    func testUploadBlossomSendsExactBytesUnderAnNMPOwnedBud11Header() async throws {
        let server = try LocalBlossomServer()
        let engine = try engineWithAccount()
        defer { engine.shutdown() }
        _ = try await engine.addAccount(secretKey: secret)
        try engine.setActiveAccount(author)

        var blob = Data("%PDF exact swift bytes\r\n".utf8)
        blob.append(contentsOf: [0x00, 0xff, 0x7f, 0x80])
        let descriptor = try await engine.uploadBlossom(
            serverURL: server.serverURL,
            blob: blob,
            contentType: "application/pdf",
            description: "Upload the signed report"
        )

        XCTAssertTrue(server.waitForRequest())
        XCTAssertEqual(server.capturedBody(), blob, "the remote must observe the exact bytes")
        XCTAssertTrue(server.capturedHead().hasPrefix("PUT /upload HTTP/1.1\r\n"))
        XCTAssertEqual(server.headerValue("content-type"), "application/pdf")

        let expected = SHA256.hash(data: blob).map { String(format: "%02x", $0) }.joined()
        XCTAssertEqual(server.headerValue("x-sha-256"), expected)
        XCTAssertEqual(descriptor.sha256, expected)
        XCTAssertEqual(descriptor.size, UInt64(blob.count))

        // The Authorization header is a signed kind:24242 the Swift caller
        // neither built nor could have influenced: it is authored by the
        // active account and bound to the hash of these exact bytes.
        let header = try XCTUnwrap(server.headerValue("authorization"))
        let encoded = try XCTUnwrap(header.split(separator: " ").last.map(String.init))
        var padded = encoded.replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        while padded.count % 4 != 0 { padded.append("=") }
        let eventData = try XCTUnwrap(Data(base64Encoded: padded))
        let event = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: eventData) as? [String: Any]
        )
        XCTAssertEqual(event["pubkey"] as? String, author)
        XCTAssertEqual(event["kind"] as? Int, 24_242)
        let tags = try XCTUnwrap(event["tags"] as? [[String]])
        XCTAssertTrue(tags.contains(["t", "upload"]))
        XCTAssertTrue(tags.contains(["x", expected]))
        XCTAssertTrue(tags.contains { $0.first == "expiration" })
    }

    /// A signer failure is typed and reaches the network zero times.
    func testUploadBlossomWithoutAnActiveSignerIsTypedAndMakesNoRequest() async throws {
        let server = try LocalBlossomServer()
        let engine = try engineWithAccount()
        defer { engine.shutdown() }
        try engine.setActiveAccount(author)

        do {
            _ = try await engine.uploadBlossom(
                serverURL: server.serverURL,
                blob: Data("no signer".utf8),
                contentType: "application/pdf",
                description: "refused"
            )
            XCTFail("an upload with no signer must fail")
        } catch let failure as BlossomUploadFailure {
            XCTAssertEqual(failure, .noActiveSigner)
        }
        XCTAssertFalse(
            server.waitForRequest(timeout: 0.5),
            "a signer refusal must not reach the server"
        )
    }

    /// Every refusal keeps its own case rather than becoming a string.
    func testUploadBlossomServerUrlRefusalIsTyped() async throws {
        let engine = try engineWithAccount()
        defer { engine.shutdown() }
        _ = try await engine.addAccount(secretKey: secret)
        try engine.setActiveAccount(author)

        do {
            _ = try await engine.uploadBlossom(
                serverURL: "ftp://blobs.example",
                blob: Data("scheme".utf8),
                contentType: "application/pdf",
                description: "refused"
            )
            XCTFail("a non-http server URL must fail")
        } catch let failure as BlossomUploadFailure {
            XCTAssertEqual(failure, .invalidServerUrl(.unsupportedScheme(scheme: "ftp")))
        }

        do {
            _ = try await engine.uploadBlossom(
                serverURL: "https://blobs.example",
                blob: Data("empty".utf8),
                contentType: "",
                description: "refused"
            )
            XCTFail("an empty content type must fail")
        } catch let failure as BlossomUploadFailure {
            XCTAssertEqual(failure, .emptyContentType)
        }
    }

    /// Swift task cancellation reaches Rust: the awaiting task is cancelled
    /// while the gated server holds the response, and the wrapper surfaces
    /// `CancellationError` rather than inventing a success or a Blossom fault.
    func testCancellingTheAwaitingTaskWithdrawsTheUpload() async throws {
        let server = try LocalBlossomServer(gated: true)
        let engine = try engineWithAccount()
        defer { engine.shutdown() }
        _ = try await engine.addAccount(secretKey: secret)
        try engine.setActiveAccount(author)

        let blob = Data("cancel during HTTP".utf8)
        let upload = Task {
            try await engine.uploadBlossom(
                serverURL: server.serverURL,
                blob: blob,
                contentType: "application/pdf",
                description: "withdrawn"
            )
        }
        XCTAssertTrue(server.waitForRequest(), "the request must reach the gated server first")
        XCTAssertEqual(server.capturedBody(), blob)
        upload.cancel()

        do {
            _ = try await upload.value
            XCTFail("a cancelled upload must not report success")
        } catch is CancellationError {
            // The observation gap: the bytes were transmitted, the local
            // operation stopped, and nothing claims what the remote did.
        }
        server.releaseResponse()
    }
}
