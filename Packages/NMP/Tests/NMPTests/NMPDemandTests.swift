// A construction/round-trip test of the ergonomic Demand descriptor (#107).
// No network -- this only proves the Swift-value <-> Ffi-value conversion
// is lossless for every ReadRouting/authenticateAs/CacheMode/Freshness case.

import XCTest
@testable import NMP
import NMPFFI

final class NMPDemandTests: XCTestCase {
    func testADemandThatNamesNoRoutingRoundTripsAsAuto() {
        let demand = NMPDemand(
            selection: NMPFilter(kinds: [1])
        )
        let ffi = demand.toFfi()
        XCTAssertEqual(ffi.routing, .auto)
        XCTAssertNil(ffi.authenticateAs)
        XCTAssertEqual(ffi.cache, .agnostic)
        XCTAssertEqual(ffi.freshness, .live)
        XCTAssertEqual(NMPDemand(ffi), demand)
    }

    func testExplicitRoutingRoundTripsWithStrictCache() {
        let demand = NMPDemand(
            selection: NMPFilter(kinds: [1]),
            routing: .explicit(["wss://relay.example.com"]),
            cache: .strict
        )
        let ffi = demand.toFfi()
        guard case .explicit(let relays) = ffi.routing else {
            return XCTFail("expected explicit routing")
        }
        XCTAssertEqual(relays, ["wss://relay.example.com"])
        XCTAssertEqual(ffi.cache, .strict)
        XCTAssertEqual(NMPDemand(ffi), demand)
    }

    func testCacheModeDefaultsToAgnosticWhenUnspecified() {
        let demand = NMPDemand(selection: NMPFilter(kinds: [1]))
        XCTAssertEqual(demand.cache, .agnostic)
        XCTAssertNil(demand.authenticateAs)
    }

    func testAuthenticateAsRoundTripsWithFrozenExpectedKey() {
        let publicKey = String(repeating: "a", count: 64)
        let demand = NMPDemand(
            selection: NMPFilter(kinds: [1]),
            routing: .explicit(["wss://relay.example.com"]),
            authenticateAs: publicKey
        )

        XCTAssertEqual(demand.toFfi().authenticateAs, publicKey)
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
                freshness: freshness
            )
            XCTAssertEqual(NMPDemand(demand.toFfi()), demand)
        }
    }
}
