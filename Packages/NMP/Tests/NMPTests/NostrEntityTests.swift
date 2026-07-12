// The bech32 nostr-entity decode codec (#116) -- construction/mapping tests
// only. No network: bech32 decoding is pure and local.

import XCTest
@testable import NMP

final class NostrEntityTests: XCTestCase {
    func testDecodesABareNpub() throws {
        let npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy"
        let entity = try decodeNostrEntity(npub)
        guard case .pubkey(let pubkey) = entity else {
            return XCTFail("expected .pubkey, got \(entity)")
        }
        XCTAssertEqual(pubkey, "aa4fc8665f5696e33db7e1a572e3b0f5b3d615837b0f362dcb1c8068b098c7b4")
    }

    func testDecodesANostrUriPrefixedEntityIdenticallyToTheBareForm() throws {
        let npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy"
        let viaUri = try decodeNostrEntity("nostr:\(npub)")
        let viaBare = try decodeNostrEntity(npub)
        XCTAssertEqual(viaUri, viaBare)
    }

    func testDecodesAnNprofileWithRelayHints() throws {
        let nprofile = "nprofile1qqsrhuxx8l9ex335q7he0f09aej04zpazpl0ne2cgukyawd24mayt8gppemhxue69uhhytnc9e3k7mf0qyt8wumn8ghj7er2vfshxtnnv9jxkc3wvdhk6tclr7lsh"
        let entity = try decodeNostrEntity(nprofile)
        guard case .profile(let pubkey, let relays) = entity else {
            return XCTFail("expected .profile, got \(entity)")
        }
        XCTAssertEqual(pubkey, "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d")
        XCTAssertEqual(relays, ["wss://r.x.com/", "wss://djbas.sadkb.com/"])
    }

    /// #116's honest-modeling requirement: `.event`'s `author`/`kind` stay
    /// independently optional, never force-fit.
    func testDecodesAnNeventWithAnAuthorButNoKindOrRelays() throws {
        let nevent = "nevent1qqsdhet4232flykq3048jzc9msmaa3hnxuesxy3lnc33vd0wt9xwk6szyqewrqnkx4zsaweutf739s0cu7et29zrntqs5elw70vlm8zudr3y24sqsgy"
        let entity = try decodeNostrEntity(nevent)
        guard case .event(let id, let author, let kind, let relays) = entity else {
            return XCTFail("expected .event, got \(entity)")
        }
        XCTAssertEqual(id, "dbe57554549f92c08bea790b05dc37dec6f3373303123f9e231635ee594ceb6a")
        XCTAssertEqual(author, "32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245")
        XCTAssertNil(kind)
        XCTAssertEqual(relays, [])
    }

    func testDecodesAnNaddrWithKindAuthorAndIdentifier() throws {
        let naddr = "naddr1qqxnzd3exgersv33xymnsve3qgs8suecw4luyht9ekff89x4uacneapk8r5dyk0gmn6uwwurf6u9rusrqsqqqa282m3gxt"
        let entity = try decodeNostrEntity(naddr)
        guard case .coordinate(let kind, let author, let identifier, let relays) = entity else {
            return XCTFail("expected .coordinate, got \(entity)")
        }
        XCTAssertEqual(kind, 30023)
        XCTAssertEqual(author, "787338757fc25d65cd929394d5e7713cf43638e8d259e8dcf5c73b834eb851f2")
        XCTAssertEqual(identifier, "1692282117831")
        XCTAssertEqual(relays, [])
    }

    /// The refusal case: a secret-key entity must never decode as if it
    /// were a display target.
    func testRejectsAnNsecRatherThanLeakingTheSecretKey() {
        let nsec = "nsec1j4c6269y9w0q2er2xjw8sv2ehyrtfxq3jwgdlxj6qfn8z4gjsq5qfvfk99"
        XCTAssertThrowsError(try decodeNostrEntity(nsec)) { error in
            XCTAssertEqual(error as? NMPError, .nostrEntitySecretKeyRejected)
        }
    }

    func testRejectsMalformedBech32WithATypedErrorNeverACrash() {
        XCTAssertThrowsError(try decodeNostrEntity("npub1notvalidbech32")) { error in
            guard case .invalidNostrEntity = error as? NMPError else {
                return XCTFail("expected .invalidNostrEntity, got \(error)")
            }
        }
    }
}
