import Foundation
@preconcurrency import Network
import CryptoKit
import XCTest
@testable import NMP

/// The fixture's own sha256 hex, computed with the platform's CryptoKit --
/// test-only, so asserting the returned descriptor's hash does not depend
/// on any NMP-owned hashing path.
private func sha256Hex(_ data: Data) -> String {
    SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
}

/// A minimal scripted HTTP/1.1 loopback server for `uploadBlossom`'s real
/// `PUT /upload` round trip (#971) -- reads the FULL request (headers AND
/// the `Content-Length` body) before replying, mirroring the Rust-side
/// scripted mock's own #538 discipline (never reply before the full request
/// is drained, or the client can observe an early close). Serves exactly
/// ONE connection and captures its method/path/body for assertion.
private final class LocalBlossomUploadServer: @unchecked Sendable {
    private let listener: NWListener
    private let queue = DispatchQueue(label: "nmp.swift.blossom-upload.fixture")
    private let accepted = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    private let responseBody: Data

    private(set) var baseURL = ""
    private var capturedMethod: String?
    private var capturedPath: String?
    private var capturedBody = Data()

    init(descriptorJSON: String) throws {
        listener = try NWListener(using: .tcp, on: .any)
        responseBody = Data(descriptorJSON.utf8)

        let ready = DispatchSemaphore(value: 0)
        listener.stateUpdateHandler = { state in
            if case .ready = state {
                ready.signal()
            }
        }
        listener.newConnectionHandler = { [weak self] connection in
            self?.serve(connection, received: Data())
        }
        listener.start(queue: queue)
        guard ready.wait(timeout: .now() + 2) == .success, let port = listener.port else {
            listener.cancel()
            throw FixtureError.listenerDidNotStart
        }
        baseURL = "http://localhost:\(port.rawValue)"
    }

    deinit {
        listener.cancel()
    }

    /// Wait for the one request this server serves, then return its
    /// method/path/body. Fails the caller's expectation if none arrived.
    func waitForRequest(timeout: TimeInterval = 2) -> (method: String, path: String, body: Data)? {
        guard accepted.wait(timeout: .now() + timeout) == .success else { return nil }
        lock.lock()
        defer { lock.unlock() }
        guard let method = capturedMethod, let path = capturedPath else { return nil }
        return (method, path, capturedBody)
    }

    private func serve(_ connection: NWConnection, received: Data) {
        connection.start(queue: queue)
        receiveUntilHeadersComplete(connection, received: received)
    }

    private func receiveUntilHeadersComplete(_ connection: NWConnection, received: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, _, error in
            guard let self else { return }
            var received = received
            if let data { received.append(data) }
            guard let headerEnd = received.range(of: Data("\r\n\r\n".utf8)) else {
                if error == nil {
                    self.receiveUntilHeadersComplete(connection, received: received)
                }
                return
            }
            let headerBytes = received[..<headerEnd.lowerBound]
            let headerText = String(decoding: headerBytes, as: UTF8.self)
            let lines = headerText.components(separatedBy: "\r\n")
            let requestLine = lines.first?.components(separatedBy: " ") ?? []
            let method = requestLine.first ?? ""
            let path = requestLine.count > 1 ? requestLine[1] : ""
            let contentLength = lines
                .first { $0.lowercased().hasPrefix("content-length:") }
                .flatMap { $0.split(separator: ":").last }
                .flatMap { Int($0.trimmingCharacters(in: .whitespaces)) } ?? 0

            let body = Data(received[headerEnd.upperBound...])
            self.receiveBodyUntilComplete(
                connection, method: method, path: path, body: body, contentLength: contentLength
            )
        }
    }

    private func receiveBodyUntilComplete(
        _ connection: NWConnection, method: String, path: String, body: Data, contentLength: Int
    ) {
        if body.count >= contentLength {
            finish(connection, method: method, path: path, body: body.prefix(contentLength))
            return
        }
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, _, error in
            guard let self else { return }
            var body = body
            if let data { body.append(data) }
            if body.count >= contentLength || error != nil {
                self.finish(connection, method: method, path: path, body: body.prefix(contentLength))
                return
            }
            self.receiveBodyUntilComplete(
                connection, method: method, path: path, body: body, contentLength: contentLength
            )
        }
    }

    private func finish(_ connection: NWConnection, method: String, path: String, body: Data) {
        lock.lock()
        capturedMethod = method
        capturedPath = path
        capturedBody = body
        lock.unlock()
        accepted.signal()

        let headers = Data(
            ("HTTP/1.1 200 OK\r\n" +
                "Content-Type: application/json\r\n" +
                "Content-Length: \(responseBody.count)\r\n" +
                "Connection: close\r\n\r\n").utf8
        )
        connection.send(content: headers + responseBody, completion: .contentProcessed { _ in
            connection.cancel()
        })
    }

    private enum FixtureError: Error {
        case listenerDidNotStart
    }
}

final class UploadBlossomTests: XCTestCase {
    @MainActor
    func testUploadBlossomPerformsTheRealOneShotSequence() async throws {
        let blob = Data("nmp swift upload_blossom reachability fixture".utf8)
        let sha256 = sha256Hex(blob)
        let url = "https://cdn.example.com/\(sha256)"
        let descriptorJSON = #"{"url":"\#(url)","sha256":"\#(sha256)","size":\#(blob.count),"type":"image/png"}"#
        let server = try LocalBlossomUploadServer(descriptorJSON: descriptorJSON)

        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        _ = try engine.session.add(privateKey: .generate(), makeCurrent: true)

        let request = Task { @MainActor in
            try await engine.uploadBlossom(
                serverURL: server.baseURL,
                blob: blob,
                contentType: "image/png",
                description: "upload_blossom Swift reachability fixture"
            )
        }
        let descriptor = try await request.value

        guard let observed = server.waitForRequest() else {
            return XCTFail("the mock server never observed a request")
        }
        XCTAssertEqual(observed.method, "PUT")
        XCTAssertEqual(observed.path, "/upload")
        XCTAssertEqual(observed.body, blob, "the exact prepared bytes were uploaded verbatim")

        XCTAssertEqual(descriptor.url, url)
        XCTAssertEqual(descriptor.sha256, sha256)
        XCTAssertEqual(descriptor.mimeType, "image/png")
    }

    @MainActor
    func testSignedOutUploadIsRefusedBeforeAnyNetworkCall() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        do {
            _ = try await engine.uploadBlossom(
                serverURL: "https://cdn.example.com",
                blob: Data("bytes".utf8),
                contentType: "image/png",
                description: "no account selected"
            )
            XCTFail("a signed-out engine must refuse before any I/O")
        } catch let error as UploadBlossomError {
            XCTAssertEqual(error, .signedOut)
        }
    }
}
