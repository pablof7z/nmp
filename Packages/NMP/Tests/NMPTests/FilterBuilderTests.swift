// A construction/round-trip test of the ergonomic query-descriptor builder
// (M4 plan §8 step D "Green"). No network -- this only proves the
// Swift-value <-> Ffi-value conversion is lossless and that the builder
// reads the way M4 plan §9's sketch intends.

import XCTest
@testable import NMP
import NMPFFI

final class FilterBuilderTests: XCTestCase {
    func testSimpleKindsFilterRoundTrips() {
        let filter = NMPFilter(kinds: [1], limit: 50)
        let ffi = filter.toFfi()
        XCTAssertEqual(ffi.kinds, [1])
        XCTAssertEqual(ffi.limit, 50)
        XCTAssertNil(ffi.authors)

        let back = NMPFilter(ffi)
        XCTAssertEqual(back, filter)
    }

    func testLiteralAuthorsBindingRoundTrips() {
        let hexPubkey = String(repeating: "a", count: 64)
        let filter = NMPFilter(kinds: [1], authors: .literal([hexPubkey]))
        let ffi = filter.toFfi()
        guard case .literal(let values) = ffi.authors else {
            return XCTFail("expected a literal binding")
        }
        XCTAssertEqual(values, [hexPubkey])
        XCTAssertEqual(NMPFilter(ffi), filter)
    }

    func testReactiveAndTagBindingRoundTrips() {
        let filter = NMPFilter(kinds: [1], tags: ["p": .reactive(.activePubkey)])
        let ffi = filter.toFfi()
        guard case .reactive(let field) = ffi.tags["p"] else {
            return XCTFail("expected a reactive binding on tag 'p'")
        }
        XCTAssertEqual(field, .activePubkey)
        XCTAssertEqual(NMPFilter(ffi), filter)
    }

    func testNIP29GroupTagRoundTrips() {
        let filter = NMPFilter(
            kinds: [9, 30_315],
            tags: ["h": .literal(["group-id"])]
        )
        let ffi = filter.toFfi()

        guard case .literal(let values) = ffi.tags["h"] else {
            return XCTFail("expected a literal binding on tag 'h'")
        }
        XCTAssertEqual(values, ["group-id"])
        XCTAssertEqual(NMPFilter(ffi), filter)
    }

    /// The follows-derivation shape from the M4 plan §9 mapping: kind:1 from
    /// authors DERIVED from my kind:3 contact list's `p` tags, i.e. "my
    /// follows' notes" -- entirely a value, no closures, no comparator code.
    func testDerivedFollowsFilterRoundTrips() {
        let follows = NMPBinding.derived(
            inner: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
            project: .tag("p")
        )
        let filter = NMPFilter(kinds: [1], authors: follows)

        let ffi = filter.toFfi()
        guard case .derived(let derived) = ffi.authors else {
            return XCTFail("expected a derived binding")
        }
        XCTAssertEqual(derived.inner().kinds, [3])
        XCTAssertEqual(NMPSelector(derived.project()), .tag("p"))
        XCTAssertEqual(NMPFilter(ffi), filter)
    }

    /// "Follows minus mutes" -- the set-algebra shape from the plan §9
    /// mapping's `mutes` example.
    func testSetOpDiffOfTwoDerivedBindingsRoundTrips() {
        let follows = NMPBinding.derived(
            inner: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
            project: .tag("p")
        )
        let mutes = NMPBinding.derived(
            inner: NMPFilter(kinds: [10_000], authors: .reactive(.activePubkey)),
            project: .tag("p")
        )
        let filter = NMPFilter(kinds: [1], authors: .setOp(.diff, [follows, mutes]))

        let ffi = filter.toFfi()
        guard case .setOp(let setOp) = ffi.authors else {
            return XCTFail("expected a setOp binding")
        }
        XCTAssertEqual(setOp.op(), .diff)
        XCTAssertEqual(setOp.operands().count, 2)
        XCTAssertEqual(NMPFilter(ffi), filter)
    }

    func testWriteIntentConversion() {
        let intent = WriteIntent(
            payload: .unsigned(
                pubkey: String(repeating: "b", count: 64),
                createdAt: 1_700_000_000,
                kind: 1,
                tags: [["t", "nostr"]],
                content: "hello from NMP"
            ),
            durability: .durable,
            routing: .authorOutbox
        )
        let ffi = intent.toFfi()
        XCTAssertEqual(ffi.durability, .durable)
        XCTAssertEqual(ffi.routing, .authorOutbox)
        guard case .unsigned(_, _, _, let tags, let content) = ffi.payload else {
            return XCTFail("expected an unsigned payload")
        }
        XCTAssertEqual(content, "hello from NMP")
        XCTAssertEqual(tags, [["t", "nostr"]])
    }

    /// #32: a `.signed` payload round-trips to `FfiWritePayload.signed`
    /// field-for-field -- the Swift mirror of the Rust
    /// `ffi_publishes_presigned_event_verbatim` test.
    func testSignedWriteIntentConversion() {
        let intent = WriteIntent(
            payload: .signed(
                id: String(repeating: "a", count: 64),
                pubkey: String(repeating: "b", count: 64),
                createdAt: 1_700_000_000,
                kind: 1,
                tags: [["e", String(repeating: "c", count: 64)]],
                content: "presigned",
                sig: String(repeating: "d", count: 128)
            ),
            durability: .durable,
            routing: .authorOutbox
        )
        let ffi = intent.toFfi()
        guard case .signed(let id, let pubkey, _, _, let tags, let content, let sig) = ffi.payload else {
            return XCTFail("expected a signed payload")
        }
        XCTAssertEqual(id, String(repeating: "a", count: 64))
        XCTAssertEqual(pubkey, String(repeating: "b", count: 64))
        XCTAssertEqual(content, "presigned")
        XCTAssertEqual(tags, [["e", String(repeating: "c", count: 64)]])
        XCTAssertEqual(sig, String(repeating: "d", count: 128))
    }
}
