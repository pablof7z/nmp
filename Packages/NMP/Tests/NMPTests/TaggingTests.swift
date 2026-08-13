// #1243: the tagging door at the native boundary.

import XCTest

@testable import NMP

final class TaggingTests: XCTestCase {
    private func row(kind: UInt16, tags: [[String]] = [], sources: [String] = []) -> Row {
        Row(
            id: String(repeating: "a", count: 64),
            pubkey: String(repeating: "b", count: 64),
            createdAt: 1_700_000_000,
            kind: kind,
            tags: tags,
            content: "body",
            signature: .signed(signature: String(repeating: "c", count: 128)),
            sources: sources
        )
    }

    /// #1243's own report, closed at the boundary it named: a native chat app
    /// composes a C7 reply through NMP instead of hand-building a row, it is
    /// kind:9, and it points with `e` rather than NIP-18's `q` quote marker.
    func testChatReplyIsKindNineAndPointsWithE() throws {
        let parent = row(kind: 9, sources: ["wss://chat.example.com"])
        guard case .event(let kind, let tags, _, _) = try chatReply(to: parent) else {
            return XCTFail("a chat reply composes an ordinary builder payload")
        }
        XCTAssertEqual(kind, 9)

        guard let eRow = tags.first(where: { $0[0] == "e" }) else {
            return XCTFail("a reply points with e")
        }
        XCTAssertEqual(eRow[1], parent.id)
        XCTAssertEqual(
            eRow[2], "wss://chat.example.com",
            "the verified source survives the boundary and fills the hint")
        XCTAssertEqual(eRow[3], parent.pubkey, "the author slot is filled")
        XCTAssertFalse(tags.contains { $0[0] == "q" }, "a reply is not a quote")
        XCTAssertFalse(tags.contains { $0[0] == "h" }, "group context is NIP-29's, never C7's")
        XCTAssertTrue(tags.contains { $0[0] == "p" && $0[1] == parent.pubkey })
    }

    /// The thread position is the wire's, across the boundary as much as in
    /// Rust: replying to a reply names the ROOT as root and the target as
    /// reply, whatever the app believed about either.
    func testReplyReadsTheTargetsOwnThreadPosition() throws {
        let rootID = String(repeating: "d", count: 64)
        let target = row(kind: 1, tags: [["e", rootID, "", "root"]], sources: ["wss://relay.example"])
        guard case .event(let kind, let tags, _, _) = try replyTo(target) else {
            return XCTFail("a reply composes an ordinary builder payload")
        }
        XCTAssertEqual(kind, 1)
        XCTAssertEqual(tags[0][1], rootID)
        XCTAssertEqual(tags[0][3], "root")
        XCTAssertEqual(tags[1][1], target.id)
        XCTAssertEqual(tags[1][3], "reply")
    }

    /// A repost names the entity, so reposting a reply reposts THAT note and
    /// never the conversation's root -- which is what a NIP-18 reader would
    /// otherwise take from a threaded row pair, since it reads the first `e`.
    func testRepostNamesTheEntityAndSplitsItsOwnKind() throws {
        let rootID = String(repeating: "e", count: 64)
        let reply = row(kind: 1, tags: [["e", rootID, "", "root"]])
        guard case .event(let kind, let tags, _, _) = try repost(reply) else {
            return XCTFail("a repost composes an ordinary builder payload")
        }
        XCTAssertEqual(kind, 6)
        let eRows = tags.filter { $0[0] == "e" }
        XCTAssertEqual(eRows.count, 1)
        XCTAssertEqual(eRows[0][1], reply.id)

        guard case .event(let genericKind, let genericTags, _, _) = try repost(row(kind: 20)) else {
            return XCTFail("a repost composes an ordinary builder payload")
        }
        XCTAssertEqual(genericKind, 16)
        XCTAssertTrue(genericTags.contains { $0[0] == "k" && $0[1] == "20" })
    }

    /// A composed draft is content-free until the app says what it says.
    func testWithContentFillsADraftWithoutDisturbingItsRows() throws {
        let draft = try chatReply(to: row(kind: 9))
        guard case .event(_, let tags, let empty, _) = draft,
            case .event(_, let sameTags, let content, _) = try draft.withContent([.text("hello")])
        else {
            return XCTFail("a chat reply composes an ordinary builder payload")
        }
        XCTAssertEqual(empty, "")
        XCTAssertEqual(content, "hello")
        XCTAssertEqual(tags, sameTags)
    }

    /// #964's remaining half: a message that is NOT a reply. Until this door
    /// crossed the boundary an app stated `kind: 9` itself for every ordinary
    /// message it sent.
    func testChatIsKindNineAndCarriesNoRows() throws {
        guard case .event(let kind, let tags, let content, let createdAt) = chat() else {
            return XCTFail("a chat composes an ordinary builder payload")
        }
        XCTAssertEqual(kind, 9)
        XCTAssertTrue(tags.isEmpty, "a chat states no policy rows")
        XCTAssertEqual(content, "")
        XCTAssertNil(createdAt, "a schema-only composer invents no timestamp")
    }

    /// The whole point of the door: the `nostr:npub…` a reader sees and the
    /// `p` row that notifies the person come out of ONE statement, so an app
    /// can no longer append `["p", hex]` by hand and hope it matches the token
    /// it separately put in the content.
    func testNamingAPersonWritesTheTokenAndThePRowTogether() throws {
        let alice = String(repeating: "b", count: 64)
        guard
            case .event(_, let tags, let content, _) = try chat().withContent([
                .text("hey "), .person(pubkey: alice, relay: nil), .text(", look"),
            ])
        else {
            return XCTFail("a named person composes an ordinary builder payload")
        }
        XCTAssertTrue(
            content.hasPrefix("hey nostr:npub1"),
            "bech32 is rendered at the user boundary: \(content)")
        XCTAssertTrue(content.hasSuffix(", look"))
        XCTAssertEqual(tags, [["p", alice]])
    }

    /// A stated relay reaches BOTH halves, because both come from the same
    /// part: the rendered pointer becomes an `nprofile` carrying the relay and
    /// the `p` row's hint cell carries the same value.
    func testAStatedRelayReachesTheTokenAndTheRowTogether() throws {
        let alice = String(repeating: "b", count: 64)
        guard
            case .event(_, let tags, let content, _) = try chat().withContent([
                .person(pubkey: alice, relay: "wss://relay.example")
            ])
        else {
            return XCTFail("a named person composes an ordinary builder payload")
        }
        XCTAssertTrue(content.hasPrefix("nostr:nprofile1"), content)
        XCTAssertEqual(tags, [["p", alice, "wss://relay.example"]])
    }

    /// An event named inline is a QUOTE, never a thread reply, and its hint
    /// comes from where NMP actually saw it -- the row's own verified sources.
    func testQuotingAnEventRendersItAndEmitsItsQRow() throws {
        let quoted = row(kind: 9, sources: ["wss://chat.example.com"])
        guard
            case .event(_, let tags, let content, _) = try chat().withContent([
                .text("look: "), .quote(quoted),
            ])
        else {
            return XCTFail("a quote composes an ordinary builder payload")
        }
        XCTAssertTrue(content.hasPrefix("look: nostr:nevent1"), content)
        XCTAssertEqual(tags, [["q", quoted.id, "wss://chat.example.com", quoted.pubkey]])
    }

    /// #155's own report, closed at the boundary it named: a native app
    /// composes a reaction through NMP instead of hand-writing `kind: 7` with
    /// its own `e` and `p` rows, and the door fills the hint, the author slot
    /// and the `k` row an app-written pair never carried.
    func testReactionIsKindSevenAndCarriesWhatTheOneDoorFills() throws {
        let target = row(kind: 1, sources: ["wss://relay.example"])
        guard case .event(let kind, let tags, let content, _) = try react(to: target, with: .like)
        else {
            return XCTFail("a reaction composes an ordinary builder payload")
        }
        XCTAssertEqual(kind, 7)
        XCTAssertEqual(content, "+")

        guard let eRow = tags.first(where: { $0[0] == "e" }) else {
            return XCTFail("a reaction points with e")
        }
        XCTAssertEqual(eRow[1], target.id)
        XCTAssertEqual(eRow[2], "wss://relay.example")
        XCTAssertEqual(eRow[3], target.pubkey)
        XCTAssertTrue(tags.contains { $0[0] == "p" && $0[1] == target.pubkey })
        XCTAssertTrue(tags.contains { $0[0] == "k" && $0[1] == "1" })
    }

    /// The three readings NIP-25 defines. An app never writes the content
    /// bytes, so it cannot spell "like" by accident.
    func testTheReactionVocabularyIsNip25sThreeReadings() throws {
        func content(_ reaction: Reaction) throws -> String {
            guard case .event(_, _, let content, _) = try react(to: row(kind: 1), with: reaction)
            else { return "<not a builder payload>" }
            return content
        }
        XCTAssertEqual(try content(.like), "+")
        XCTAssertEqual(try content(.dislike), "-")
        XCTAssertEqual(try content(.emoji("🔥")), "🔥")
    }

    /// NIP-25 says there MUST always be an `e` tag set to the id of the event
    /// being reacted to, so reacting to a reply names the REPLY -- a client
    /// tallying by the first `e` cannot credit the thread root with a reaction
    /// nobody gave it.
    func testReactingToAReplyNamesTheReplyAndNeverItsRoot() throws {
        let rootID = String(repeating: "f", count: 64)
        let reply = row(kind: 1, tags: [["e", rootID, "", "root"]])
        guard case .event(_, let tags, _, _) = try react(to: reply, with: .like) else {
            return XCTFail("a reaction composes an ordinary builder payload")
        }
        let eRows = tags.filter { $0[0] == "e" }
        XCTAssertEqual(eRows.count, 1)
        XCTAssertEqual(eRows[0][1], reply.id)
    }

    /// Both refusals are typed and synchronous: an empty emoji is NIP-25's
    /// spelling of a LIKE, and a NIP-30 `:shortcode:` needs a companion `emoji`
    /// row this door does not write.
    func testAnEmojiThatWouldSaySomethingElseRefuses() throws {
        for emoji in ["", ":soapbox:"] {
            XCTAssertThrowsError(try react(to: row(kind: 1), with: .emoji(emoji))) {
                guard case NMPError.invalidReaction = $0 else {
                    return XCTFail("expected a typed reaction refusal, got \($0)")
                }
            }
        }
    }

    /// A malformed key is a typed refusal; nothing partial escapes.
    func testAMalformedNamedKeyRefuses() throws {
        XCTAssertThrowsError(try chat().withContent([.person(pubkey: "not-a-key", relay: nil)])) {
            guard case NMPError.invalidPublicKey = $0 else {
                return XCTFail("expected a typed key refusal, got \($0)")
            }
        }
    }
}
