import NMPContent
import NMPUI
import SwiftUI
import XCTest

final class NMPUITests: XCTestCase {
    func testNameFallsBackAndPrefersDisplayName() {
        let pubkey = String(repeating: "ab", count: 32)
        XCTAssertEqual(NMPDisplayName.abbreviatedPubkey(pubkey), "ababababab…ababab")
        XCTAssertEqual(NMPDisplayName.resolve(pubkey: pubkey, profile: nil), "ababababab…ababab")

        let profile = NostrProfileMetadata(pubkey: pubkey, name: "alice", displayName: "Alice A.")
        XCTAssertEqual(NMPDisplayName.resolve(pubkey: pubkey, profile: profile), "Alice A.")
    }

    func testReadingTimeIsBoundedAndConfigurable() {
        XCTAssertEqual(NMPReadingTime.minutes(for: ""), 1)
        XCTAssertEqual(NMPReadingTime.minutes(for: Array(repeating: "word", count: 440).joined(separator: " ")), 2)
        XCTAssertEqual(NMPReadingTime.minutes(for: "one two three", wordsPerMinute: 2), 2)
    }

    func testRendererSetAcceptsAppOnlyKindWithoutCoreChange() {
        let custom = NostrContentRenderers.standard.event(kind: 12_938) { _ in
            Text("app-only")
        }
        XCTAssertTrue(custom.hasEventRenderer(kind: 12_938))
        XCTAssertFalse(NostrContentRenderers.standard.hasEventRenderer(kind: 12_938))
        XCTAssertTrue(NostrContentRenderers.standard.hasEventRenderer(kind: 30_023))
    }

    func testAvatarFallbackColorIsStable() {
        let pubkey = String(repeating: "01", count: 32)
        XCTAssertEqual(
            String(describing: NMPAvatar.placeholderColor(for: pubkey)),
            String(describing: NMPAvatar.placeholderColor(for: pubkey))
        )
    }

    func testArticleLeafTextUsesStableFallbacks() {
        let article = NostrArticle(
            eventID: "event",
            author: "author",
            createdAt: 1,
            identifier: "article",
            title: "   ",
            summary: "  useful summary  ",
            content: "one two three"
        )
        XCTAssertEqual(NMPArticleText.title(article), "Untitled article")
        XCTAssertEqual(NMPArticleText.summary(article), "useful summary")
    }

    func testAvatarGroupReportsOverflowDeterministically() {
        let people = (0..<6).map { index in
            NMPAvatarItem(pubkey: String(repeating: "\(index)", count: 64))
        }
        let group = NMPAvatarGroup(people: people, maximumVisible: 4)
        XCTAssertEqual(group.overflowCount, 2)
    }

    func testCompactCountsMatchReactionComponents() {
        XCTAssertEqual(NMPCompactCount.string(for: 999), "999")
        XCTAssertEqual(NMPCompactCount.string(for: 1_250), "1.2k")
    }
}
