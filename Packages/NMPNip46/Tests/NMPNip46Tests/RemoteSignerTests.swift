import NMP
import XCTest
@testable import NMPNip46

final class RemoteSignerTests: XCTestCase {
    func testCatalogContainsOnlyNip46DetectionLaunchAndPackageFacts() {
        let primal = NMPNip46SignerDiscovery.known.first { $0.id == "primal" }
        XCTAssertEqual(primal?.iosDetectionURI, "primalconnect://probe")
        XCTAssertEqual(primal?.nip46LaunchScheme, "primalconnect")
        XCTAssertEqual(primal?.androidDetectionURI, "primal://signer")
        XCTAssertEqual(primal?.androidPackageID, "net.primal.android")
        XCTAssertEqual(NMPNip46SignerDiscovery.known.map(\.id), ["primal"])
    }

    func testInjectedIOSProbeNeverInventsAmberAndFindsPrimalByExactURI() {
        var probed: [String] = []
        let installed = NMPNip46SignerDiscovery.matchingIOSApps { url in
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
        let primal = try XCTUnwrap(NMPNip46SignerDiscovery.known.first { $0.id == "primal" })
        let appSpecific = try invitation.uri(for: primal)
        XCTAssertTrue(generic.hasPrefix("nostrconnect://"))
        XCTAssertTrue(appSpecific.hasPrefix("primalconnect://"))
        XCTAssertEqual(
            generic.dropFirst("nostrconnect".count),
            appSpecific.dropFirst("primalconnect".count)
        )
    }

    func testObserverProjectsReadyThenFinishesOnClose() async {
        let observer = NIP46Observer()
        var states = observer.stream.makeAsyncIterator()

        observer.onReady(userPublicKey: "user-key")
        observer.onClosed()

        let ready = await states.next()
        let completed = await states.next()
        XCTAssertEqual(ready, .ready(userPublicKey: "user-key"))
        XCTAssertNil(completed)
    }

    func testConnectionCloseAndDeinitAreIdempotentAndStreamScoped() {
        let lock = NSLock()
        var closedA = 0
        var closedB = 0
        var connectionA: NMPNip46Connection? = NMPNip46Connection(
            observer: NIP46Observer(),
            closeAction: {
                lock.withLock { closedA += 1 }
            }
        )
        var connectionB: NMPNip46Connection? = NMPNip46Connection(
            observer: NIP46Observer(),
            closeAction: {
                lock.withLock { closedB += 1 }
            }
        )

        connectionA?.close()
        connectionA?.close()
        connectionA = nil
        XCTAssertEqual(lock.withLock { closedA }, 1)
        XCTAssertEqual(lock.withLock { closedB }, 0)

        connectionB?.close()
        connectionB = nil
        XCTAssertEqual(lock.withLock { closedB }, 1)
    }
}
