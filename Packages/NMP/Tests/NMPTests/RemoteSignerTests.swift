import XCTest
@testable import NMP

final class RemoteSignerTests: XCTestCase {
    func testCatalogSeparatesDetectionLaunchPackageAndProviderFacts() {
        let primal = NMPLocalSignerDiscovery.known.first { $0.id == "primal" }
        XCTAssertEqual(primal?.iosDetectionURI, "primalconnect://probe")
        XCTAssertEqual(primal?.nip46LaunchScheme, "primalconnect")
        XCTAssertEqual(primal?.androidDetectionURI, "primal://signer")
        XCTAssertEqual(primal?.androidPackageID, "net.primal.android")
        XCTAssertEqual(primal?.androidProviderAuthority, "net.primal.android")
        XCTAssertEqual(primal?.protocols, [.nip46, .nip55])

        let amber = NMPLocalSignerDiscovery.known.first { $0.id == "amber" }
        XCTAssertNil(amber?.iosDetectionURI)
        XCTAssertEqual(amber?.protocols, [.nip55])
    }

    func testInjectedIOSProbeNeverInventsAmberAndFindsPrimalByExactURI() {
        var probed: [String] = []
        let installed = NMPLocalSignerDiscovery.matchingIOSApps { url in
            probed.append(url.absoluteString)
            return url.absoluteString == "primalconnect://probe"
        }
        XCTAssertEqual(installed.map(\.id), ["primal"])
        XCTAssertEqual(probed, ["primalconnect://probe"])
    }

    func testPrimalInvitationUsesAppSpecificHandoffWithoutChangingPayload() throws {
        let engine = try NMPEngine(config: .init())
        defer { engine.shutdown() }
        let invitation = try engine.nip46Invitation(relays: ["wss://relay.example"])
        let generic = try invitation.uri()
        let primal = try XCTUnwrap(NMPLocalSignerDiscovery.known.first { $0.id == "primal" })
        let appSpecific = try invitation.uri(for: primal)
        XCTAssertTrue(generic.hasPrefix("nostrconnect://"))
        XCTAssertTrue(appSpecific.hasPrefix("primalconnect://"))
        XCTAssertEqual(
            generic.dropFirst("nostrconnect".count),
            appSpecific.dropFirst("primalconnect".count)
        )
    }
}
