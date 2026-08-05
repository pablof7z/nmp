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
            sig: String(repeating: "c", count: 128),
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
            case .event(_, let sameTags, let content, _) = draft.withContent("hello")
        else {
            return XCTFail("a chat reply composes an ordinary builder payload")
        }
        XCTAssertEqual(empty, "")
        XCTAssertEqual(content, "hello")
        XCTAssertEqual(tags, sameTags)
    }
}
