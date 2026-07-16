// A construction/round-trip test of the ergonomic Demand descriptor (#107).
// No network -- this only proves the Swift-value <-> Ffi-value conversion
// is lossless for every SourceAuthority/AccessContext/CacheMode/Freshness case.

import XCTest
@testable import NMP
import NMPFFI

final class NMPDemandTests: XCTestCase {
    func testAuthorOutboxesSourceRoundTrips() {
        let demand = NMPDemand(
            selection: NMPFilter(kinds: [1]),
            source: .authorOutboxes
        )
        let ffi = demand.toFfi()
        XCTAssertEqual(ffi.source, .authorOutboxes)
        XCTAssertEqual(ffi.access, .public)
        XCTAssertEqual(ffi.cache, .agnostic)
        XCTAssertEqual(ffi.freshness, .live)
        XCTAssertEqual(NMPDemand(ffi), demand)
    }

    func testPinnedSourceRoundTripsWithStrictCache() {
        let demand = NMPDemand(
            selection: NMPFilter(kinds: [1]),
            source: .pinned(["wss://relay.example.com"]),
            cache: .strict
        )
        let ffi = demand.toFfi()
        guard case .pinned(let relays) = ffi.source else {
            return XCTFail("expected a pinned source")
        }
        XCTAssertEqual(relays, ["wss://relay.example.com"])
        XCTAssertEqual(ffi.cache, .strict)
        XCTAssertEqual(NMPDemand(ffi), demand)
    }

    func testCacheModeDefaultsToAgnosticWhenUnspecified() {
        let demand = NMPDemand(selection: NMPFilter(kinds: [1]), source: .public)
        XCTAssertEqual(demand.cache, .agnostic)
        XCTAssertEqual(demand.access, .public)
    }

    func testNip42AccessContextRoundTripsWithFrozenExpectedKey() {
        let publicKey = String(repeating: "a", count: 64)
        let demand = NMPDemand(
            selection: NMPFilter(kinds: [1]),
            source: .pinned(["wss://relay.example.com"]),
            access: .nip42(publicKey: publicKey)
        )

        XCTAssertEqual(demand.toFfi().access, .nip42(publicKey: publicKey))
        XCTAssertEqual(NMPDemand(demand.toFfi()), demand)
    }

    func testFreshnessRoundTripsEveryWholeSecondVariant() {
        for freshness in [
            NMPFreshness.live,
            .maxAge(seconds: 14_400),
            .cacheOnly,
        ] {
            let demand = NMPDemand(
                selection: NMPFilter(kinds: [0]),
                source: .authorOutboxes,
                freshness: freshness
            )
            XCTAssertEqual(NMPDemand(demand.toFfi()), demand)
        }
    }
}
