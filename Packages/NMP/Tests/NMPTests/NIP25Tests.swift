import XCTest
@testable import NMP
import NMPFFI

final class NIP25Tests: XCTestCase {
    private enum TestFailure: Error {
        case noSignedEvent
        case timeout
    }

    private let secret = String(repeating: "0", count: 63) + "1"

    private func seedCanonicalTarget(_ engine: NMPEngine) async throws -> String {
        let account = try await engine.addAccount(secretKey: secret)
        try engine.setActiveAccount(account.publicKey)
        let receipt = try await engine.publish(
            WriteIntent(
                payload: .unsigned(
                    pubkey: account.publicKey,
                    createdAt: 42,
                    kind: 1,
                    tags: [],
                    content: "canonical target"
                ),
                durability: .durable,
                routing: .authorOutbox
            )
        )
        return try await withThrowingTaskGroup(of: String.self) { group in
            group.addTask {
                for try await status in receipt.status {
                    if case .signed(let eventID) = status {
                        return eventID
                    }
                }
                throw TestFailure.noSignedEvent
            }
            group.addTask {
                try await Task.sleep(nanoseconds: 5_000_000_000)
                throw TestFailure.timeout
            }
            guard let eventID = try await group.next() else {
                throw TestFailure.noSignedEvent
            }
            group.cancelAll()
            return eventID
        }
    }

    private func fabricatedRow(id: String) -> Row {
        Row(
            id: id,
            pubkey: String(repeating: "f", count: 64),
            createdAt: .max,
            kind: 65_535,
            tags: [["h", "attacker-group"], ["e", String(repeating: "a", count: 64)]],
            content: "native-forged body",
            sig: String(repeating: "0", count: 128),
            sources: ["wss://attacker.invalid"]
        )
    }

    func testCallerConstructibleRowCanOnlySelectCanonicalEventID() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let eventID = try await seedCanonicalTarget(engine)

        // All fields except id are intentionally contradictory. Success
        // proves the wrapper forwards only id; Rust re-reads the canonical
        // signed row and its canonical provenance.
        let target = try engine.reactionTarget(for: fabricatedRow(id: eventID))
        _ = try engine.reactionDraft(target: target, value: .like)
        _ = try engine.reactionDraft(
            target: target,
            value: .customEmoji(
                shortcode: "soapbox",
                imageURL: "https://cdn.example/soapbox.png"
            )
        )
    }

    func testMalformedUnknownSignedOutAndInvalidValueAreTypedRefusals() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        XCTAssertThrowsError(try engine.reactionTarget(for: fabricatedRow(id: "bad"))) { error in
            XCTAssertEqual(error as? ReactionError, .invalidEventID(got: "bad"))
        }

        let unknown = String(repeating: "7", count: 64)
        XCTAssertThrowsError(
            try engine.reactionTarget(for: fabricatedRow(id: unknown))
        ) { error in
            XCTAssertEqual(error as? ReactionError, .targetNotFound(eventID: unknown))
        }

        let eventID = try await seedCanonicalTarget(engine)
        let target = try engine.reactionTarget(for: fabricatedRow(id: eventID))
        try engine.setActiveAccount(nil)
        XCTAssertThrowsError(
            try engine.reactionDraft(target: target, value: .like)
        ) { error in
            XCTAssertEqual(error as? ReactionError, .noActiveAccount)
        }

        let account = try await engine.addAccount(secretKey: secret)
        try engine.setActiveAccount(account.publicKey)
        XCTAssertThrowsError(
            try engine.reactionDraft(target: target, value: .emoji(":missing:"))
        ) { error in
            XCTAssertEqual(
                error as? ReactionError,
                .customEmojiRequiresMetadata(got: ":missing:")
            )
        }
    }

    func testEveryFFIFailureKeepsItsTypedNativeAxis() {
        let cases: [(FfiReactionError, ReactionError)] = [
            (.InvalidEventId(got: "id"), .invalidEventID(got: "id")),
            (.TargetNotFound(eventId: "id"), .targetNotFound(eventID: "id")),
            (.TargetNotVerified(eventId: "id"), .targetNotVerified(eventID: "id")),
            (
                .CanonicalLookupUnavailable(reason: "closed"),
                .canonicalLookupUnavailable(reason: "closed")
            ),
            (.EngineClosed, .engineClosed),
            (.NoActiveAccount, .noActiveAccount),
            (.EmptyEmoji, .emptyEmoji),
            (
                .StandardValueRequiresTypedVariant(got: "+"),
                .standardValueRequiresTypedVariant(got: "+")
            ),
            (
                .CustomEmojiRequiresMetadata(got: ":x:"),
                .customEmojiRequiresMetadata(got: ":x:")
            ),
            (.InvalidEmojiToken(got: "two words"), .invalidEmojiToken(got: "two words")),
            (
                .InvalidCustomEmojiShortcode(got: "bad!"),
                .invalidCustomEmojiShortcode(got: "bad!")
            ),
            (
                .InvalidCustomEmojiUrl(got: "file:///x"),
                .invalidCustomEmojiURL(got: "file:///x")
            ),
        ]
        for (ffi, expected) in cases {
            XCTAssertEqual(ReactionError(ffi), expected)
        }
    }
}
