import NMP
import NMPContent
import XCTest

final class ContentDocumentTests: XCTestCase {
    private let npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy"

    func testPlainTextKeepsOccurrenceAndNormalizesProfile() {
        let document = parseNostrContent("hello nostr:\(npub)")

        XCTAssertEqual(document.references.count, 1)
        let occurrence = try! XCTUnwrap(document.references.first)
        XCTAssertEqual(occurrence.original, "nostr:\(npub)")
        XCTAssertEqual(occurrence.placement, .inline)
        guard case .profile(let pubkey, let hints) = occurrence.target else {
            return XCTFail("expected normalized profile target")
        }
        XCTAssertEqual(pubkey, "aa4fc8665f5696e33db7e1a572e3b0f5b3d615837b0f362dcb1c8068b098c7b4")
        XCTAssertTrue(hints.isEmpty)
    }

    func testStandaloneReferenceIsNotForcedIntoInlinePresentation() {
        let document = parseNostrContent("nostr:\(npub)")
        XCTAssertEqual(document.references.first?.placement, .standalone)
    }

    func testMarkdownContextIsSemanticNotAViewCatalog() {
        let document = parseNostrContent("# Heading\n\nhello **world**", syntax: .markdown)
        XCTAssertEqual(document.blocks.count, 2)
        XCTAssertEqual(document.blocks.first?.context, .heading(level: 1))
        XCTAssertEqual(document.blocks.last?.context, .paragraph)
    }

    func testReferenceDemandIsAnOrdinaryNMPDemand() {
        let target = NostrReferenceTarget.profile(
            pubkey: "aa4fcb665f5696e33db7e1a572e3b0f5b3d615837b0f362dcb1c8068b098c7b4"
        )
        let plan = referenceDemandPlan(for: target)
        XCTAssertEqual(plan.targetKey, "profile:aa4fcb665f5696e33db7e1a572e3b0f5b3d615837b0f362dcb1c8068b098c7b4")
        XCTAssertEqual(plan.canonical.source, .authorOutboxes)
        XCTAssertEqual(plan.canonical.selection.kinds, [0])
        XCTAssertTrue(plan.helpers.isEmpty)
    }
}
