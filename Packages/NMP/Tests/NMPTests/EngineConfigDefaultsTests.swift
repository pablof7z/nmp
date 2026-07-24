import XCTest
@testable import NMP
import NMPFFI

final class EngineConfigDefaultsTests: XCTestCase {
    func testErgonomicDefaultsComeFromGeneratedBoundary() {
        let generated = NmpEngineConfig()
        let ergonomic = NMPConfig()

        XCTAssertEqual(ergonomic.storePath, generated.storePath)
        XCTAssertEqual(ergonomic.indexerRelays, generated.indexerRelays)
        XCTAssertEqual(ergonomic.appRelays, generated.appRelays)
        XCTAssertEqual(ergonomic.fallbackRelays, generated.fallbackRelays)
        XCTAssertEqual(
            ergonomic.allowedLocalRelayHosts,
            generated.allowedLocalRelayHosts
        )
        XCTAssertEqual(ergonomic.maxRelays, generated.maxRelays)
        XCTAssertEqual(
            ergonomic.maxAuthCapabilities,
            generated.maxAuthCapabilities
        )
    }

    func testExplicitZeroKeepsItsDistinctFieldSemantics() {
        let config = NMPConfig(maxRelays: 0, maxAuthCapabilities: 0)

        XCTAssertEqual(config.toFfi().maxRelays, 0)
        XCTAssertEqual(config.toFfi().maxAuthCapabilities, 0)
    }
}
