import Foundation
import NMP
import NMPContent
import XCTest

final class ReferenceFixtureParityTests: XCTestCase {
    func testSharedNIP19FixturesPreserveExactSwiftLocators() throws {
        for fixture in try loadReferenceFixtures().cases {
            switch fixture.outcome {
            case "public":
                let expected = try XCTUnwrap(fixture.locator, fixture.name)
                XCTAssertEqual(
                    normalize(try decodeNostrEntity(fixture.input)),
                    expected,
                    "\(fixture.name) decoded entity"
                )
                if fixture.input.hasPrefix("nostr:") {
                    XCTAssertEqual(
                        normalize(try decodeNostrEntity(String(fixture.input.dropFirst(6)))),
                        expected,
                        "\(fixture.name) nostr URI and bare forms"
                    )
                }
                let occurrence = try XCTUnwrap(
                    parseNostrContent(fixture.input).references.single,
                    fixture.name
                )
                XCTAssertEqual(
                    normalize(occurrence.target),
                    expected,
                    "\(fixture.name) content locator"
                )
            case "secret_key", "malformed":
                try assertNonActionable(fixture)
            default:
                XCTFail("unknown shared fixture outcome \(fixture.outcome)")
            }
        }
    }

    private func assertNonActionable(_ fixture: ReferenceFixture) throws {
        let document = parseNostrContent(fixture.input)
        XCTAssertTrue(document.references.isEmpty, fixture.name)
        XCTAssertEqual(document.visibleText, fixture.input, fixture.name)

        switch fixture.outcome {
        case "secret_key":
            XCTAssertThrowsError(try decodeNostrEntity(fixture.input)) { error in
                XCTAssertEqual(error as? NMPError, .nostrEntitySecretKeyRejected)
            }
        case "malformed":
            XCTAssertThrowsError(try decodeNostrEntity(fixture.input)) { error in
                guard case .invalidNostrEntity = error as? NMPError else {
                    return XCTFail("expected invalidNostrEntity, got \(error)")
                }
            }
        default:
            XCTFail("expected a non-actionable fixture")
        }
    }
}

private struct ReferenceFixtureCorpus: Decodable {
    let schema: UInt16
    let cases: [ReferenceFixture]
}

private struct ReferenceFixture: Decodable {
    let name: String
    let input: String
    let outcome: String
    let locator: NormalizedNostrEntity?
}

private struct NormalizedNostrEntity: Decodable, Equatable {
    let variant: String
    let pubkey: String?
    let id: String?
    let author: String?
    let eventKind: UInt16?
    let identifier: String?
    let relays: [String]
}

private func loadReferenceFixtures() throws -> ReferenceFixtureCorpus {
    var repository = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
    for _ in 0..<4 {
        repository = repository.deletingLastPathComponent()
    }
    let data = try Data(
        contentsOf: repository.appendingPathComponent("fixtures/reference-locators.json")
    )
    let decoder = JSONDecoder()
    decoder.keyDecodingStrategy = .convertFromSnakeCase
    let corpus = try decoder.decode(ReferenceFixtureCorpus.self, from: data)
    XCTAssertEqual(corpus.schema, 2)
    return corpus
}

private func normalize(_ target: NostrReferenceTarget) -> NormalizedNostrEntity {
    switch target {
    case .pubkey(let pubkey):
        return normalized("pubkey", pubkey: pubkey)
    case .profile(let pubkey, let relays):
        return normalized("profile", pubkey: pubkey, relays: relays)
    case .eventID(let id):
        return normalized("event_id", id: id)
    case .event(let id, let author, let kind, let relays):
        return normalized("event", id: id, author: author, eventKind: kind, relays: relays)
    case .coordinate(let kind, let author, let identifier, let relays):
        return normalized(
            "coordinate",
            author: author,
            eventKind: kind,
            identifier: identifier,
            relays: relays
        )
    }
}

private func normalize(_ entity: NostrEntity) -> NormalizedNostrEntity {
    switch entity {
    case .pubkey(let pubkey):
        return normalized("pubkey", pubkey: pubkey)
    case .profile(let pubkey, let relays):
        return normalized("profile", pubkey: pubkey, relays: relays)
    case .eventId(let id):
        return normalized("event_id", id: id)
    case .event(let id, let author, let kind, let relays):
        return normalized("event", id: id, author: author, eventKind: kind, relays: relays)
    case .coordinate(let kind, let author, let identifier, let relays):
        return normalized(
            "coordinate",
            author: author,
            eventKind: kind,
            identifier: identifier,
            relays: relays
        )
    }
}

private func normalized(
    _ variant: String,
    pubkey: String? = nil,
    id: String? = nil,
    author: String? = nil,
    eventKind: UInt16? = nil,
    identifier: String? = nil,
    relays: [String] = []
) -> NormalizedNostrEntity {
    NormalizedNostrEntity(
        variant: variant,
        pubkey: pubkey,
        id: id,
        author: author,
        eventKind: eventKind,
        identifier: identifier,
        relays: relays
    )
}

private extension Array {
    var single: Element? {
        count == 1 ? first : nil
    }
}

private extension NostrContentDocument {
    var visibleText: String {
        blocks.flatMap(\.inlines).compactMap { inline in
            guard case .text(let text, _, _) = inline else { return nil }
            return text
        }.joined()
    }
}
