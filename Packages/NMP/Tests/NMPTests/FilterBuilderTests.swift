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
            inner: NMPDemand(
                selection: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
                source: .authorOutboxes
            ),
            project: .tag("p")
        )
        let filter = NMPFilter(kinds: [1], authors: follows)

        let ffi = filter.toFfi()
        guard case .derived(let derived) = ffi.authors else {
            return XCTFail("expected a derived binding")
        }
        XCTAssertEqual(derived.inner().selection.kinds, [3])
        XCTAssertEqual(NMPSelector(derived.project()), .tag("p"))
        XCTAssertEqual(NMPFilter(ffi), filter)
    }

    func testDerivedInnerFullDemandRoundTripsEveryPolicyIndependently() {
        let inner = NMPDemand(
            selection: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
            source: .pinned(["wss://inner.example.com"]),
            access: .nip42(publicKey: String(repeating: "a", count: 64)),
            cache: .strict,
            freshness: .maxAge(seconds: 600)
        )
        let filter = NMPFilter(
            kinds: [1],
            authors: .derived(inner: inner, project: .tag("p"))
        )

        let ffi = filter.toFfi()
        guard case .derived(let derived) = ffi.authors else {
            return XCTFail("expected a derived binding")
        }
        XCTAssertEqual(NMPDemand(derived.inner()), inner)
        XCTAssertEqual(NMPFilter(ffi), filter)

        var publicInner = inner
        publicInner.access = .public
        let sameSelectionDifferentContext = NMPFilter(
            kinds: [1],
            authors: .derived(inner: publicInner, project: .tag("p"))
        )
        XCTAssertNotEqual(filter, sameSelectionDifferentContext)
        XCTAssertEqual(
            NMPFilter(sameSelectionDifferentContext.toFfi()),
            sameSelectionDifferentContext
        )
    }

    /// "Follows minus mutes" -- the set-algebra shape from the plan §9
    /// mapping's `mutes` example.
    func testSetOpDiffOfTwoDerivedBindingsRoundTrips() {
        let follows = NMPBinding.derived(
            inner: NMPDemand(
                selection: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
                source: .authorOutboxes
            ),
            project: .tag("p")
        )
        let mutes = NMPBinding.derived(
            inner: NMPDemand(
                selection: NMPFilter(kinds: [10_000], authors: .reactive(.activePubkey)),
                source: .authorOutboxes
            ),
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
            payload: .event(
                kind: 1,
                tags: [["t", "nostr"]],
                content: "hello from NMP",
                createdAt: 1_700_000_000
            ),
            routing: .auto
        )
        let ffi = intent.toFfi()
        XCTAssertEqual(ffi.routing, .auto)
        guard case .event(let builder) = ffi.payload else {
            return XCTFail("expected a builder payload")
        }
        XCTAssertEqual(builder.content, "hello from NMP")
        XCTAssertEqual(builder.tags, [["t", "nostr"]])
        XCTAssertEqual(builder.createdAt, 1_700_000_000)
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
            routing: .auto
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

    /// #47: an explicit identity crosses to `FfiWriteIntent` intact -- the
    /// per-write identity is data, never rewritten or dropped by the mirror.
    func testWriteIntentConversionCarriesAnExplicitIdentity() {
        let named = String(repeating: "b", count: 64)
        let intent = WriteIntent(
            payload: .event(
                kind: 1,
                tags: [],
                content: "as the named identity",
                createdAt: 1_700_000_000
            ),
            routing: .auto,
            identity: .explicit(pubkey: named)
        )
        XCTAssertEqual(intent.toFfi().identity, .explicit(pubkey: named))
    }

    /// #47: naming nobody is not the absence of a choice -- the default init
    /// means `.active`, "whoever is active at acceptance", all the way
    /// through `toFfi()`. There is no third "unset" state to observe.
    func testWriteIntentDefaultInitMeansTheActiveAccount() {
        let intent = WriteIntent(
            payload: .event(
                kind: 1,
                tags: [],
                content: "active-account default",
                createdAt: 1_700_000_000
            ),
            routing: .auto
        )
        XCTAssertEqual(intent.identity, .active)
        XCTAssertEqual(intent.toFfi().identity, .active)
    }

    func testWriteIntentReverseProjectionPreservesEveryGenericField() {
        let composed = WriteIntent(
            FfiWriteIntent(
                payload: .event(
                    builder: FfiEventBuilder(
                        kind: 1111,
                        tags: [["I", "podcast:item:guid:42"]],
                        content: "composed",
                        createdAt: 42
                    )
                ),
                routing: .auto,
                identity: .explicit(pubkey: String(repeating: "a", count: 64)),
                correlation: "correlation-42"
            )
        )
        XCTAssertEqual(
            composed.payload,
            WritePayload.event(
                kind: 1111,
                tags: [["I", "podcast:item:guid:42"]],
                content: "composed",
                createdAt: 42
            )
        )
        XCTAssertEqual(composed.routing, .auto)
        XCTAssertEqual(composed.identity, .explicit(pubkey: String(repeating: "a", count: 64)))
        XCTAssertEqual(composed.correlation, "correlation-42")

        let signed = WriteIntent(
            FfiWriteIntent(
                payload: .signed(
                    id: String(repeating: "b", count: 64),
                    pubkey: String(repeating: "c", count: 64),
                    createdAt: 43,
                    kind: 1,
                    tags: [["e", String(repeating: "d", count: 64)]],
                    content: "signed",
                    sig: String(repeating: "e", count: 128)
                ),
                routing: .auto,
                identity: .active,
                correlation: nil
            )
        )
        XCTAssertEqual(
            signed.payload,
            .signed(
                id: String(repeating: "b", count: 64),
                pubkey: String(repeating: "c", count: 64),
                createdAt: 43,
                kind: 1,
                tags: [["e", String(repeating: "d", count: 64)]],
                content: "signed",
                sig: String(repeating: "e", count: 128)
            )
        )
        XCTAssertEqual(signed.routing, .auto)
        XCTAssertEqual(signed.identity, .active)
        XCTAssertNil(signed.correlation)
    }

    /// #972: a Swift app can name the exact relays a write goes to -- the
    /// relay list a user typed into a text field crosses the boundary
    /// verbatim, in order, and comes back unchanged.
    func testExplicitRoutingCarriesTheAppsExactRelayListBothWays() {
        let typed = ["wss://user-typed-relay.example", "wss://second.example"]
        let intent = WriteIntent(
            payload: .event(kind: 1, content: "for the archive", createdAt: 42),
            routing: .explicit(relays: typed)
        )
        XCTAssertEqual(intent.toFfi().routing, .explicit(relays: typed))

        let back = WriteIntent(
            FfiWriteIntent(
                payload: .event(
                    builder: FfiEventBuilder(
                        kind: 1, tags: [], content: "for the archive", createdAt: 42)
                ),
                    routing: .explicit(relays: typed),
                identity: .active,
                correlation: nil
            )
        )
        XCTAssertEqual(back.routing, .explicit(relays: typed))
    }
}
