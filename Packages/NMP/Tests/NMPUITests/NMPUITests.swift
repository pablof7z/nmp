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
}
