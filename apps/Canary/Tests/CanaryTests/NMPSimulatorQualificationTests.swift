import XCTest
import NMP

final class NMPSimulatorQualificationTests: XCTestCase {
    /// A real iOS Simulator process must resolve a public relay hostname with
    /// the platform HTTP stack and return its NIP-11 document through the
    /// supported Swift facade. This is intentionally not a loopback fixture:
    /// it falsifies the custom-resolver failure that broke public iOS NIP-11.
    @MainActor
    func testPublicHostnameUsesPlatformDNSOnIOSSimulator() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        let information = try await engine.relayInformation(
            for: "wss://relay.damus.io",
            policy: .refresh
        )

        XCTAssertEqual(information.document.name, "damus.io")
        XCTAssertTrue(information.document.supportedNips?.contains(11) == true)
        XCTAssertEqual(information.documentRevision.count, 64)
    }
}
