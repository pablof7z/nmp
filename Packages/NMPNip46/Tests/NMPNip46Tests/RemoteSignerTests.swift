@_spi(NMPProviderComponents) import NMP
import CryptoKit
import Darwin
import Foundation
import NMPNip46FFI
import XCTest
@testable import NMPNip46

private final class NativeCloseRecord: @unchecked Sendable {
    private let lock = NSLock()
    private var action: (@Sendable () -> Bool?)?
    private(set) var count = 0
    private(set) var installationCloseResults: [Bool?] = []

    func setAction(_ action: @escaping @Sendable () -> Bool?) {
        lock.withLock {
            self.action = action
        }
    }

    func recordClose() {
        lock.withLock {
            count += 1
            installationCloseResults.append(action?())
        }
    }

    func snapshot() -> (count: Int, results: [Bool?]) {
        lock.withLock {
            (count, installationCloseResults)
        }
    }
}

private final class CheckpointOutcome: @unchecked Sendable {
    private let lock = NSLock()
    private var error: Error?

    func set(error: Error) {
        lock.withLock {
            self.error = error
        }
    }

    func get() -> Error? {
        lock.withLock { error }
    }
}

private final class ForwardingNip46Observer: Nip46ConnectionObserver, @unchecked Sendable {
    let projected: NIP46Observer
    let closeRecord: NativeCloseRecord

    init(projected: NIP46Observer, closeRecord: NativeCloseRecord) {
        self.projected = projected
        self.closeRecord = closeRecord
    }

    func onEvent(event: FfiNip46ConnectionEvent) {
        projected.onEvent(event: event)
    }

    func onReady(userPublicKey: String) {
        projected.onReady(userPublicKey: userPublicKey)
    }

    func onFailed(failure: FfiNip46Failure) {
        projected.onFailed(failure: failure)
    }

    func onClosed() {
        closeRecord.recordClose()
        projected.onClosed()
    }
}

private final class TCPBlackhole: @unchecked Sendable {
    private let descriptor: Int32
    private let lock = NSLock()
    private var clients: [Int32] = []
    private var source: DispatchSourceRead!
    private(set) var relay = ""

    init() throws {
        descriptor = socket(AF_INET, SOCK_STREAM, 0)
        guard descriptor >= 0 else {
            throw POSIXError(.ENOTSOCK)
        }

        var reuse: Int32 = 1
        guard setsockopt(
            descriptor,
            SOL_SOCKET,
            SO_REUSEADDR,
            &reuse,
            socklen_t(MemoryLayout<Int32>.size)
        ) == 0 else {
            Darwin.close(descriptor)
            throw POSIXError(.EADDRINUSE)
        }

        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = 0
        address.sin_addr = in_addr(s_addr: inet_addr("127.0.0.1"))
        let bound = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(
                    descriptor,
                    $0,
                    socklen_t(MemoryLayout<sockaddr_in>.size)
                )
            }
        }
        guard bound == 0, listen(descriptor, SOMAXCONN) == 0 else {
            Darwin.close(descriptor)
            throw POSIXError(.EADDRINUSE)
        }

        var length = socklen_t(MemoryLayout<sockaddr_in>.size)
        let named = withUnsafeMutablePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                getsockname(descriptor, $0, &length)
            }
        }
        guard named == 0 else {
            Darwin.close(descriptor)
            throw POSIXError(.EINVAL)
        }
        relay = "ws://127.0.0.1:\(UInt16(bigEndian: address.sin_port))"

        source = DispatchSource.makeReadSource(
            fileDescriptor: descriptor,
            queue: DispatchQueue(label: "nmp.nip46.blackhole")
        )
        source.setEventHandler { [weak self] in
            self?.acceptClient()
        }
        source.resume()
    }

    private func acceptClient() {
        let client = Darwin.accept(descriptor, nil, nil)
        guard client >= 0 else { return }
        guard completeWebSocketHandshake(client) else {
            Darwin.close(client)
            return
        }
        lock.withLock {
            clients.append(client)
        }
    }

    private func completeWebSocketHandshake(_ client: Int32) -> Bool {
        let marker = Data("\r\n\r\n".utf8)
        var request = Data()
        var buffer = [UInt8](repeating: 0, count: 2_048)
        while request.range(of: marker) == nil, request.count < 16_384 {
            let count = Darwin.recv(client, &buffer, buffer.count, 0)
            guard count > 0 else { return false }
            request.append(buffer, count: count)
        }
        guard
            let requestText = String(data: request, encoding: .utf8),
            let keyLine = requestText
                .components(separatedBy: "\r\n")
                .first(where: { $0.lowercased().hasPrefix("sec-websocket-key:") })
        else { return false }
        let key = keyLine
            .split(separator: ":", maxSplits: 1)
            .last?
            .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        guard !key.isEmpty else { return false }
        let digest = Insecure.SHA1.hash(
            data: Data((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").utf8)
        )
        let accept = Data(digest).base64EncodedString()
        let response = Data(
            (
                "HTTP/1.1 101 Switching Protocols\r\n" +
                    "Upgrade: websocket\r\n" +
                    "Connection: Upgrade\r\n" +
                    "Sec-WebSocket-Accept: \(accept)\r\n\r\n"
            ).utf8
        )
        return response.withUnsafeBytes { rawBuffer in
            guard let base = rawBuffer.baseAddress else { return false }
            var sent = 0
            while sent < rawBuffer.count {
                let count = Darwin.send(
                    client,
                    base.advanced(by: sent),
                    rawBuffer.count - sent,
                    0
                )
                guard count > 0 else { return false }
                sent += count
            }
            return true
        }
    }

    var acceptedCount: Int {
        lock.withLock { clients.count }
    }

    deinit {
        source.cancel()
        Darwin.close(descriptor)
        let current = lock.withLock {
            let current = clients
            clients.removeAll()
            return current
        }
        current.forEach { Darwin.close($0) }
    }
}

final class RemoteSignerTests: XCTestCase {
    private let remoteSignerPublicKey =
        "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    private func compatibility() throws -> FfiNip46Compatibility {
        try withVerifiedNip46Component(
            packagedProviderIdentity: nmpNip46PackagedComponentIdentity,
            loadedProviderIdentity: nmpNip46ComponentIdentity(),
            packagedInterfaceIdentity: nmpProviderComponentInterfaceIdentity(),
            loadedCoreIdentity: nmpProviderCoreComponentIdentity()
        ) { $0 }
    }

    private func bunkerURI(relay: String) throws -> String {
        var components = URLComponents()
        components.scheme = "bunker"
        components.host = remoteSignerPublicKey
        components.queryItems = [
            URLQueryItem(name: "relay", value: relay),
            URLQueryItem(name: "secret", value: "nmp-native-test"),
        ]
        return try XCTUnwrap(components.string)
    }

    private func prepare(
        blackhole: TCPBlackhole,
        compatibility: FfiNip46Compatibility,
        observer: Nip46ConnectionObserver
    ) throws -> FfiNip46PreparedConnection {
        try prepareNip46Bunker(
            compatibility: compatibility,
            bunkerUri: bunkerURI(relay: blackhole.relay),
            timeoutMillis: 30_000,
            observer: observer
        )
    }

    private func eventually(
        timeout: TimeInterval = 2,
        _ predicate: () -> Bool
    ) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while !predicate(), Date() < deadline {
            Thread.sleep(forTimeInterval: 0.01)
        }
        return predicate()
    }

    private func finishedStates(
        from stream: AsyncStream<NMPNip46ConnectionState>,
        timeout: Duration = .seconds(3)
    ) async -> [NMPNip46ConnectionState]? {
        await withTaskGroup(of: [NMPNip46ConnectionState]?.self) { group in
            group.addTask {
                var states: [NMPNip46ConnectionState] = []
                for await state in stream {
                    states.append(state)
                }
                return states
            }
            group.addTask {
                try? await Task.sleep(for: timeout)
                return nil
            }
            let first = await group.next() ?? nil
            group.cancelAll()
            return first
        }
    }

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

    func testMismatchedCoreIsTypedBeforeAdapterPreparationRuns() {
        var adapterPreparationRan = false

        XCTAssertThrowsError(
            try withVerifiedNip46Component(
                packagedProviderIdentity: nmpNip46PackagedComponentIdentity,
                loadedProviderIdentity: nmpNip46ComponentIdentity(),
                packagedInterfaceIdentity: nmpProviderComponentInterfaceIdentity(),
                loadedCoreIdentity: "deliberately-mismatched-core"
            ) { _ in
                adapterPreparationRan = true
            }
        ) { error in
            guard case .nativeComponentMismatch(
                component: "nmp-nip46",
                expectedIdentity: let expected,
                actualIdentity: "deliberately-mismatched-core"
            ) = error as? NMPError else {
                return XCTFail("unexpected error: \(error)")
            }
            XCTAssertTrue(expected.hasPrefix("nmp-core-component-v2-"))
        }
        XCTAssertFalse(adapterPreparationRan)
    }

    func testMismatchedPackagedInterfaceIsTypedBeforeAdapterPreparationRuns() {
        var adapterPreparationRan = false

        XCTAssertThrowsError(
            try withVerifiedNip46Component(
                packagedProviderIdentity: nmpNip46PackagedComponentIdentity,
                loadedProviderIdentity: nmpNip46ComponentIdentity(),
                packagedInterfaceIdentity: "deliberately-mismatched-interface",
                loadedCoreIdentity: nmpProviderCoreComponentIdentity()
            ) { _ in
                adapterPreparationRan = true
            }
        ) { error in
            guard case .nativeComponentMismatch(
                component: "nmp-nip46",
                expectedIdentity: let expected,
                actualIdentity: "deliberately-mismatched-interface"
            ) = error as? NMPError else {
                return XCTFail("unexpected error: \(error)")
            }
            XCTAssertTrue(expected.hasPrefix("nmp-component-interface-v2-"))
        }
        XCTAssertFalse(adapterPreparationRan)
    }

    func testMismatchedPackagedProviderBindingIsTypedBeforeAdapterPreparationRuns() {
        var adapterPreparationRan = false

        XCTAssertThrowsError(
            try withVerifiedNip46Component(
                packagedProviderIdentity: "deliberately-mismatched-binding",
                loadedProviderIdentity: nmpNip46ComponentIdentity(),
                packagedInterfaceIdentity: nmpProviderComponentInterfaceIdentity(),
                loadedCoreIdentity: nmpProviderCoreComponentIdentity()
            ) { _ in
                adapterPreparationRan = true
            }
        ) { error in
            guard case .nativeComponentMismatch(
                component: "nmp-nip46",
                expectedIdentity: let expected,
                actualIdentity: "deliberately-mismatched-binding"
            ) = error as? NMPError else {
                return XCTFail("unexpected error: \(error)")
            }
            XCTAssertTrue(expected.hasPrefix("nmp-nip46-component-v2-"))
        }
        XCTAssertFalse(adapterPreparationRan)
    }

    func testMismatchedLoadedProviderNativeIsTypedBeforeAdapterPreparationRuns() {
        var adapterPreparationRan = false

        XCTAssertThrowsError(
            try withVerifiedNip46Component(
                packagedProviderIdentity: nmpNip46PackagedComponentIdentity,
                loadedProviderIdentity: "deliberately-mismatched-native",
                packagedInterfaceIdentity: nmpProviderComponentInterfaceIdentity(),
                loadedCoreIdentity: nmpProviderCoreComponentIdentity()
            ) { _ in
                adapterPreparationRan = true
            }
        ) { error in
            guard case .nativeComponentMismatch(
                component: "nmp-nip46",
                expectedIdentity: let expected,
                actualIdentity: "deliberately-mismatched-native"
            ) = error as? NMPError else {
                return XCTFail("unexpected error: \(error)")
            }
            XCTAssertTrue(expected.hasPrefix("nmp-nip46-component-v2-"))
        }
        XCTAssertFalse(adapterPreparationRan)
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

    func testPublicConnectionUsesCoreRuntimeThroughAcceptedTimeoutAndFinishedState() async throws {
        let blackhole = try TCPBlackhole()
        let engine = try NMPEngine(config: .init())
        defer { engine.shutdown() }
        let connection = try engine.connectNip46(
            bunkerURI: bunkerURI(relay: blackhole.relay),
            timeout: .milliseconds(100)
        )

        XCTAssertTrue(
            eventually { blackhole.acceptedCount == 1 },
            "the separately linked provider must reach a real socket on the core runtime"
        )
        let finished = await finishedStates(from: connection.states)
        let states = try XCTUnwrap(
            finished,
            "the provider must report a terminal result and finish its public stream"
        )
        XCTAssertEqual(states.first, .connecting)
        XCTAssertTrue(
            states.contains(.available),
            "the child-owned session worker must complete the WebSocket handshake; states=\(states)"
        )
        XCTAssertEqual(states.last, .failed(.timeout))

        connection.close()
        connection.close()
    }

    func testNativeResourcesRetainBothHandlesAndCloseInstallationBeforeProvider() throws {
        let blackhole = try TCPBlackhole()
        let proof = try compatibility()
        let projected = NIP46Observer()
        let closeRecord = NativeCloseRecord()
        let forwarding = ForwardingNip46Observer(
            projected: projected,
            closeRecord: closeRecord
        )
        let engine = try NMPEngine(config: .init())
        defer { engine.shutdown() }

        var prepared: FfiNip46PreparedConnection? = try prepare(
            blackhole: blackhole,
            compatibility: proof,
            observer: forwarding
        )
        var installation: NMPProviderSignerInstallation? = try engine
            .installSignerProviderAdapter(
                try XCTUnwrap(prepared).adapter(compatibility: proof)
            )
        weak var weakPrepared = prepared
        weak var weakInstallation = installation
        closeRecord.setAction { [weak installation] in
            installation?.close()
        }
        let connection = NMPNip46Connection(
            observer: projected,
            prepared: try XCTUnwrap(prepared),
            installation: try XCTUnwrap(installation)
        )
        prepared = nil
        installation = nil

        XCTAssertTrue(
            eventually { blackhole.acceptedCount == 1 },
            "the separately linked provider task must run on the exact core-owned runtime"
        )
        XCTAssertThrowsError(try connection.checkpoint()) { error in
            guard case .invalidSigner(let reason) = error as? NMPError else {
                return XCTFail("unexpected pre-ready checkpoint error: \(error)")
            }
            XCTAssertNotEqual(reason, "no underlying NIP-46 connection to checkpoint")
        }

        connection.close()
        connection.close()
        XCTAssertTrue(eventually { closeRecord.snapshot().count == 1 })
        XCTAssertEqual(closeRecord.snapshot().results, [false])
        XCTAssertNil(weakPrepared)
        XCTAssertNil(weakInstallation)
        XCTAssertThrowsError(try connection.checkpoint()) { error in
            XCTAssertEqual(
                error as? NMPError,
                .invalidSigner("no underlying NIP-46 connection to checkpoint")
            )
        }
    }

    func testEngineClosedCleanupLeavesAdapterUntakenForFreshInstall() throws {
        let blackhole = try TCPBlackhole()
        let proof = try compatibility()
        let projected = NIP46Observer()
        let closeRecord = NativeCloseRecord()
        let forwarding = ForwardingNip46Observer(
            projected: projected,
            closeRecord: closeRecord
        )
        let prepared = try prepare(
            blackhole: blackhole,
            compatibility: proof,
            observer: forwarding
        )
        let closedEngine = try NMPEngine(config: .init())
        closedEngine.shutdown()

        XCTAssertThrowsError(
            try closedEngine.prepareAndInstallNip46(observer: projected) { _ in prepared }
        ) { error in
            XCTAssertEqual(error as? NMPError, .engineClosed)
        }
        XCTAssertTrue(eventually { closeRecord.snapshot().count == 1 })

        let freshEngine = try NMPEngine(config: .init())
        defer { freshEngine.shutdown() }
        let installation = try freshEngine.installSignerProviderAdapter(
            prepared.adapter(compatibility: proof)
        )
        XCTAssertTrue(installation.close())
    }

    func testPreparedAliasReplayDoesNotCloseOrInvalidateFirstInstallation() throws {
        let blackhole = try TCPBlackhole()
        let proof = try compatibility()
        let projected = NIP46Observer()
        let closeRecord = NativeCloseRecord()
        let forwarding = ForwardingNip46Observer(
            projected: projected,
            closeRecord: closeRecord
        )
        let prepared = try prepare(
            blackhole: blackhole,
            compatibility: proof,
            observer: forwarding
        )
        let engine = try NMPEngine(config: .init())
        defer { engine.shutdown() }
        let first = try engine.installSignerProviderAdapter(
            prepared.adapter(compatibility: proof)
        )
        closeRecord.setAction { [weak first] in first?.close() }

        XCTAssertThrowsError(
            try engine.installSignerProviderAdapter(
                prepared.adapter(compatibility: proof)
            )
        ) { error in
            XCTAssertEqual(
                error as? NMPProviderSignerInstallError,
                .adapterAlreadyTaken
            )
        }
        XCTAssertEqual(closeRecord.snapshot().count, 0)
        XCTAssertTrue(first.close())
        prepared.connection().disconnect()
        XCTAssertTrue(eventually { closeRecord.snapshot().count == 1 })
        XCTAssertEqual(closeRecord.snapshot().results, [false])
    }

    func testCheckpointCloseRaceHasOnlyNativeNotReadyOrConsumedWrapperOutcome() throws {
        let blackhole = try TCPBlackhole()
        let proof = try compatibility()

        for _ in 0..<12 {
            let projected = NIP46Observer()
            let closeRecord = NativeCloseRecord()
            let forwarding = ForwardingNip46Observer(
                projected: projected,
                closeRecord: closeRecord
            )
            let engine = try NMPEngine(config: .init())
            let prepared = try prepare(
                blackhole: blackhole,
                compatibility: proof,
                observer: forwarding
            )
            let installation = try engine.installSignerProviderAdapter(
                prepared.adapter(compatibility: proof)
            )
            closeRecord.setAction { [weak installation] in installation?.close() }
            let connection = NMPNip46Connection(
                observer: projected,
                prepared: prepared,
                installation: installation
            )
            let start = DispatchSemaphore(value: 0)
            let group = DispatchGroup()
            let outcome = CheckpointOutcome()

            group.enter()
            DispatchQueue.global().async {
                start.wait()
                do {
                    _ = try connection.checkpoint()
                } catch {
                    outcome.set(error: error)
                }
                group.leave()
            }
            group.enter()
            DispatchQueue.global().async {
                start.wait()
                connection.close()
                group.leave()
            }
            start.signal()
            start.signal()
            XCTAssertEqual(group.wait(timeout: .now() + 2), .success)
            connection.close()
            XCTAssertTrue(eventually { closeRecord.snapshot().count == 1 })
            XCTAssertEqual(closeRecord.snapshot().results, [false])
            guard let error = outcome.get() as? NMPError,
                  case .invalidSigner = error else {
                XCTFail("race produced an unexpected checkpoint outcome")
                engine.shutdown()
                continue
            }
            XCTAssertThrowsError(try connection.checkpoint())
            engine.shutdown()
        }
    }
}
