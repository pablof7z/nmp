import Foundation
import NMP
import NMPContent
import NMPUI
import XCTest

final class ReferenceFixtureParityTests: XCTestCase {
    func testSharedNIP19FixturesMatchSwiftTargetsAndDemandPlans() throws {
        for fixture in try loadReferenceFixtures().cases {
            switch fixture.outcome {
            case "public":
                let expectedTarget = try XCTUnwrap(fixture.target, fixture.name)
                let expectedPlan = try XCTUnwrap(fixture.plan, fixture.name)
                XCTAssertEqual(
                    normalize(try decodeNostrEntity(fixture.input)),
                    expectedTarget,
                    "\(fixture.name) decoded entity"
                )
                if fixture.input.hasPrefix("nostr:") {
                    XCTAssertEqual(
                        normalize(try decodeNostrEntity(String(fixture.input.dropFirst(6)))),
                        expectedTarget,
                        "\(fixture.name) nostr URI and bare forms"
                    )
                }
                let document = parseNostrContent(fixture.input)
                let occurrence = try XCTUnwrap(document.references.single, fixture.name)
                let target = normalize(occurrence.target)
                let plan = try normalize(referenceDemandPlan(for: occurrence.target))

                XCTAssertEqual(target, expectedTarget, "\(fixture.name) target")
                XCTAssertEqual(plan, expectedPlan, "\(fixture.name) demand plan")
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
    let target: NormalizedReferenceTarget?
    let plan: NormalizedReferencePlan?
}

private struct NormalizedReferenceTarget: Decodable, Equatable {
    let kind: String
    let key: String
    let pubkey: String?
    let id: String?
    let authorHint: String?
    let kindHint: UInt16?
    let addressKind: UInt16?
    let author: String?
    let identifier: String?
    let relayHints: [String]
}

private struct NormalizedReferencePlan: Decodable, Equatable {
    let targetKey: String
    let canonical: NormalizedDemand
    let helpers: [NormalizedDemand]
    let discardedRelayHints: UInt32
}

private struct NormalizedDemand: Decodable, Equatable {
    let selection: NormalizedFilter
    let source: NormalizedSource
    let access: String
    let cache: String
}

private struct NormalizedFilter: Decodable, Equatable {
    let kinds: [UInt16]
    let authors: [String]
    let ids: [String]
    let tags: [String: [String]]
    let since: UInt64?
    let until: UInt64?
    let limit: UInt32?
}

private struct NormalizedSource: Decodable, Equatable {
    let kind: String
    let relays: [String]
}

private enum ReferenceNormalizationError: Error {
    case nonLiteralBinding
}

private func loadReferenceFixtures() throws -> ReferenceFixtureCorpus {
    var repository = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
    for _ in 0..<4 {
        repository = repository.deletingLastPathComponent()
    }
    let data = try Data(
        contentsOf: repository.appendingPathComponent("fixtures/reference-plans.json")
    )
    let decoder = JSONDecoder()
    decoder.keyDecodingStrategy = .convertFromSnakeCase
    let corpus = try decoder.decode(ReferenceFixtureCorpus.self, from: data)
    XCTAssertEqual(corpus.schema, 1)
    return corpus
}

private func normalize(_ target: NostrReferenceTarget) -> NormalizedReferenceTarget {
    switch target {
    case .profile(let pubkey, let relayHints):
        return NormalizedReferenceTarget(
            kind: "profile",
            key: target.key,
            pubkey: pubkey,
            id: nil,
            authorHint: nil,
            kindHint: nil,
            addressKind: nil,
            author: nil,
            identifier: nil,
            relayHints: relayHints
        )
    case .event(let id, let authorHint, let kindHint, let relayHints):
        return NormalizedReferenceTarget(
            kind: "event",
            key: target.key,
            pubkey: nil,
            id: id,
            authorHint: authorHint,
            kindHint: kindHint,
            addressKind: nil,
            author: nil,
            identifier: nil,
            relayHints: relayHints
        )
    case .address(let kind, let author, let identifier, let relayHints):
        return NormalizedReferenceTarget(
            kind: "address",
            key: target.key,
            pubkey: nil,
            id: nil,
            authorHint: nil,
            kindHint: nil,
            addressKind: kind,
            author: author,
            identifier: identifier,
            relayHints: relayHints
        )
    }
}

private func normalize(_ entity: NostrEntity) -> NormalizedReferenceTarget {
    switch entity {
    case .pubkey(let pubkey):
        return normalize(NostrReferenceTarget.profile(pubkey: pubkey))
    case .profile(let pubkey, let relays):
        return normalize(NostrReferenceTarget.profile(pubkey: pubkey, relayHints: relays))
    case .eventId(let id):
        return normalize(NostrReferenceTarget.event(id: id))
    case .event(let id, let author, let kind, let relays):
        return normalize(
            NostrReferenceTarget.event(
                id: id,
                authorHint: author,
                kindHint: kind,
                relayHints: relays
            )
        )
    case .coordinate(let kind, let author, let identifier, let relays):
        return normalize(
            NostrReferenceTarget.address(
                kind: kind,
                author: author,
                identifier: identifier,
                relayHints: relays
            )
        )
    }
}

private func normalize(_ plan: NostrReferenceDemandPlan) throws -> NormalizedReferencePlan {
    NormalizedReferencePlan(
        targetKey: plan.targetKey,
        canonical: try normalize(plan.canonical),
        helpers: try plan.helpers.map(normalize),
        discardedRelayHints: plan.discardedRelayHints
    )
}

private func normalize(_ demand: NMPDemand) throws -> NormalizedDemand {
    let source: NormalizedSource
    switch demand.source {
    case .authorOutboxes:
        source = NormalizedSource(kind: "author_outboxes", relays: [])
    case .public:
        source = NormalizedSource(kind: "public", relays: [])
    case .pinned(let relays):
        source = NormalizedSource(kind: "pinned", relays: relays.sorted())
    }

    let access: String
    switch demand.access {
    case .public:
        access = "public"
    case .nip42(let publicKey):
        access = "nip42:\(publicKey)"
    }

    let cache: String
    switch demand.cache {
    case .agnostic:
        cache = "agnostic"
    case .strict:
        cache = "strict"
    }

    return NormalizedDemand(
        selection: NormalizedFilter(
            kinds: (demand.selection.kinds ?? []).sorted(),
            authors: try literals(demand.selection.authors),
            ids: try literals(demand.selection.ids),
            tags: try Dictionary(
                uniqueKeysWithValues: demand.selection.tags.map { name, binding in
                    (String(name), try literals(binding))
                }
            ),
            since: demand.selection.since,
            until: demand.selection.until,
            limit: demand.selection.limit
        ),
        source: source,
        access: access,
        cache: cache
    )
}

private func literals(_ binding: NMPBinding?) throws -> [String] {
    guard let binding else { return [] }
    guard case .literal(let values) = binding else {
        throw ReferenceNormalizationError.nonLiteralBinding
    }
    return values.sorted()
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
