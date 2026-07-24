import Foundation
@preconcurrency import Network
import XCTest
@testable import NMP
import NMPFFI

private struct MediaParityFixture: Decodable {
    struct Descriptor: Decodable {
        let url: String
        let sha256: String
        let size: UInt64
        let mimeType: String

        enum CodingKeys: String, CodingKey {
            case url, sha256, size
            case mimeType = "mime_type"
        }
    }

    struct Image: Decodable {
        let width: UInt32
        let height: UInt32
        let alt: String
        let blurhash: String
        let thumbhash: String
        let fallbacks: [String]
    }

    struct Post: Decodable {
        let title: String
        let description: String
        let contentWarningReason: String
        let hashtags: [String]

        enum CodingKeys: String, CodingKey {
            case title, description, hashtags
            case contentWarningReason = "content_warning_reason"
        }
    }

    struct Expected: Decodable {
        let kind: UInt16
        let tags: [[String]]
        let content: String
        let eventId: String
        let unsignedEventJSON: String
        let durability: String
        let routing: String

        enum CodingKeys: String, CodingKey {
            case kind, tags, content, durability, routing
            case eventId = "event_id"
            case unsignedEventJSON = "unsigned_event_json"
        }
    }

    let secretKeyHex: String
    let authorPubkeyHex: String
    let createdAt: UInt64
    let blobUtf8: String
    let descriptor: Descriptor
    let image: Image
    let post: Post
    let expected: Expected

    enum CodingKeys: String, CodingKey {
        case descriptor, image, post, expected
        case secretKeyHex = "secret_key_hex"
        case authorPubkeyHex = "author_pubkey_hex"
        case createdAt = "created_at"
        case blobUtf8 = "blob_utf8"
    }

    static func load() throws -> Self {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 {
            root.deleteLastPathComponent()
        }
        let data = try Data(
            contentsOf: root.appendingPathComponent("fixtures/nip68-media-parity.json")
        )
        return try JSONDecoder().decode(Self.self, from: data)
    }
}

/// One-request HTTP/1.1 Blossom fixture. It drains the complete request body
/// before replying, avoiding the early-close reset class documented by #538.
private final class LocalBlossomServer: @unchecked Sendable {
    private let listener: NWListener
    private let queue = DispatchQueue(label: "nmp.swift.media.fixture")
    private let requestReceived = DispatchSemaphore(value: 0)
    private let responseBody: Data

    private(set) var serverURL = ""

    init(
        descriptor: MediaParityFixture.Descriptor,
        sha256Override: String? = nil
    ) throws {
        responseBody = try JSONSerialization.data(
            withJSONObject: [
                "url": descriptor.url,
                "sha256": sha256Override ?? descriptor.sha256,
                "size": descriptor.size,
                "type": descriptor.mimeType,
            ],
            options: [.sortedKeys]
        )
        listener = try NWListener(using: .tcp, on: .any)

        let ready = DispatchSemaphore(value: 0)
        listener.stateUpdateHandler = { state in
            if case .ready = state {
                ready.signal()
            }
        }
        listener.newConnectionHandler = { [weak self] connection in
            guard let self else { return }
            connection.start(queue: self.queue)
            self.receive(connection, accumulated: Data())
        }
        listener.start(queue: queue)
        guard ready.wait(timeout: .now() + 2) == .success, let port = listener.port else {
            listener.cancel()
            throw FixtureError.listenerDidNotStart
        }
        serverURL = "http://127.0.0.1:\(port.rawValue)"
    }

    deinit {
        listener.cancel()
    }

    func didReceiveRequest() -> Bool {
        requestReceived.wait(timeout: .now() + 2) == .success
    }

    private func receive(_ connection: NWConnection, accumulated: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, complete, error in
            guard let self else { return }
            var accumulated = accumulated
            if let data {
                accumulated.append(data)
            }
            if !self.isCompleteRequest(accumulated), !complete, error == nil {
                self.receive(connection, accumulated: accumulated)
                return
            }
            guard self.isCompleteRequest(accumulated) else {
                connection.cancel()
                return
            }

            self.requestReceived.signal()
            let headers = Data(
                ("HTTP/1.1 200 OK\r\n" +
                    "Content-Type: application/json\r\n" +
                    "Content-Length: \(self.responseBody.count)\r\n" +
                    "Connection: close\r\n\r\n").utf8
            )
            connection.send(
                content: headers + self.responseBody,
                completion: .contentProcessed { _ in connection.cancel() }
            )
        }
    }

    private func isCompleteRequest(_ data: Data) -> Bool {
        let terminator = Data("\r\n\r\n".utf8)
        guard let boundary = data.range(of: terminator) else { return false }
        let headerData = data[..<boundary.lowerBound]
        guard let text = String(data: headerData, encoding: .utf8) else { return false }
        let contentLength = text
            .components(separatedBy: "\r\n")
            .compactMap { line -> Int? in
                let pieces = line.split(separator: ":", maxSplits: 1)
                guard pieces.count == 2,
                      pieces[0].trimmingCharacters(in: .whitespaces)
                        .caseInsensitiveCompare("content-length") == .orderedSame
                else { return nil }
                return Int(pieces[1].trimmingCharacters(in: .whitespaces))
            }
            .first ?? 0
        return data.distance(from: boundary.upperBound, to: data.endIndex) >= contentLength
    }

    private enum FixtureError: Error {
        case listenerDidNotStart
    }
}

final class MediaTests: XCTestCase {
    /// Shared fixture falsifier: native upload retains an opaque witness, and
    /// Swift produces exactly the same governed kind/tags/content and typed
    /// write inputs as Rust/FFI/Kotlin.
    func testSharedVerifiedUploadPictureFixture() async throws {
        let fixture = try MediaParityFixture.load()
        let blob = Data(fixture.blobUtf8.utf8)
        let now = UInt64(Date().timeIntervalSince1970)

        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let registration = try await engine.addAccount(secretKey: fixture.secretKeyHex)
        XCTAssertEqual(registration.publicKey, fixture.authorPubkeyHex)
        try engine.setActiveAccount(registration.publicKey)

        let authorizationDraft = try blossomUploadAuthorizationDraft(
            authorPubkeyHex: fixture.authorPubkeyHex,
            blobSha256Hex: fixture.descriptor.sha256,
            createdAt: now - 5,
            expiration: now + 300,
            description: "NIP-68 parity upload"
        )
        let signedAuthorization = try await engine.signEvent(authorizationDraft.signRequest)
        let authorization = try BlossomAuthorization.validate(
            signedEvent: signedAuthorization,
            verb: .upload,
            blobSha256Hex: fixture.descriptor.sha256,
            now: now
        )

        let server = try LocalBlossomServer(descriptor: fixture.descriptor)
        let client = BlossomClient(
            config: BlossomClientConfig(
                allowedLocalHosts: ["127.0.0.1"],
                requestDeadlineSeconds: 5
            )
        )
        let verified = try await client.upload(
            serverURL: server.serverURL,
            blob: blob,
            contentType: fixture.descriptor.mimeType,
            authorization: authorization
        )
        XCTAssertTrue(server.didReceiveRequest())
        XCTAssertEqual(verified.descriptor.sha256, fixture.descriptor.sha256)
        XCTAssertEqual(verified.descriptor.url, fixture.descriptor.url)

        let tamperedHash = String(repeating: "0", count: 64)
        let tamperedServer = try LocalBlossomServer(
            descriptor: fixture.descriptor,
            sha256Override: tamperedHash
        )
        do {
            _ = try await client.upload(
                serverURL: tamperedServer.serverURL,
                blob: blob,
                contentType: fixture.descriptor.mimeType,
                authorization: authorization
            )
            XCTFail("tampered descriptor must not mint a VerifiedUpload")
        } catch let error as BlossomUploadError {
            XCTAssertEqual(
                error,
                .sha256Mismatch(
                    expectedSha256Hex: fixture.descriptor.sha256,
                    returnedSha256Hex: tamperedHash
                )
            )
        }
        XCTAssertTrue(tamperedServer.didReceiveRequest())

        let draft = try composePicture(
            authorPubkeyHex: fixture.authorPubkeyHex,
            createdAt: fixture.createdAt,
            images: [
                PictureImage(
                    upload: verified,
                    dimensions: ImageDimensions(
                        width: fixture.image.width,
                        height: fixture.image.height
                    ),
                    alt: fixture.image.alt,
                    blurhash: fixture.image.blurhash,
                    thumbhash: fixture.image.thumbhash,
                    fallbacks: fixture.image.fallbacks
                )
            ],
            post: PicturePost(
                title: fixture.post.title,
                description: fixture.post.description,
                contentWarning: PictureContentWarning(
                    reason: fixture.post.contentWarningReason
                ),
                hashtags: fixture.post.hashtags
            )
        )

        XCTAssertEqual(draft.kind, fixture.expected.kind)
        XCTAssertEqual(draft.tags, fixture.expected.tags)
        XCTAssertEqual(draft.content, fixture.expected.content)
        XCTAssertEqual(draft.unsignedEventJSON, fixture.expected.unsignedEventJSON)
        XCTAssertEqual(draft.signRequest.kind, fixture.expected.kind)
        XCTAssertEqual(draft.signRequest.tags, fixture.expected.tags)

        let intent = draft.writeIntent(
            durability: .durable,
            routing: .authorOutbox,
            correlation: "nip68-parity"
        )
        XCTAssertEqual(fixture.expected.durability, "durable")
        XCTAssertEqual(fixture.expected.routing, "author_outbox")
        XCTAssertEqual(intent.durability, .durable)
        XCTAssertEqual(intent.routing, .authorOutbox)
        XCTAssertEqual(intent.correlation, "nip68-parity")
        guard case .unsigned(
            let pubkey, let createdAt, let kind, let tags, let content
        ) = intent.payload else {
            return XCTFail("picture draft must produce an unsigned ordinary intent")
        }
        XCTAssertEqual(pubkey, fixture.authorPubkeyHex)
        XCTAssertEqual(createdAt, fixture.createdAt)
        XCTAssertEqual(kind, fixture.expected.kind)
        XCTAssertEqual(tags, fixture.expected.tags)
        XCTAssertEqual(content, fixture.expected.content)

        let signedPicture = try await engine.signEvent(draft.signRequest)
        XCTAssertEqual(signedPicture.id, fixture.expected.eventId)
        XCTAssertEqual(signedPicture.pubkey, fixture.authorPubkeyHex)
        XCTAssertEqual(signedPicture.kind, fixture.expected.kind)
        XCTAssertEqual(signedPicture.tags, fixture.expected.tags)
        XCTAssertEqual(signedPicture.content, fixture.expected.content)
    }

    func testEveryComposeFailureMapsWithoutCollapsingDomains() {
        XCTAssertEqual(
            PictureComposeError(.InvalidAuthorPubkey(got: "bad")),
            .invalidAuthorPubkey(got: "bad")
        )
        XCTAssertEqual(PictureComposeError(.NoImages), .noImages)
        XCTAssertEqual(PictureComposeError(.ImageMissingMimeType), .imageMissingMimeType)
        XCTAssertEqual(PictureComposeError(.EmptyHashtag), .emptyHashtag)
    }
}
