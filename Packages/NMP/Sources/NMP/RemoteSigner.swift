import Foundation
import NMPFFI

#if canImport(UIKit)
import UIKit
#endif

public enum NMPLocalSignerProtocol: Sendable, Hashable {
    case nip46
    case nip55
}

/// Rust-owned discovery facts for one local signer app. Detection URI,
/// handoff scheme, Android package, provider authority, and protocol support
/// remain distinct so a shared URL scheme never becomes an identity claim.
public struct NMPLocalSigner: Sendable, Hashable, Identifiable {
    public let id: String
    public let displayName: String
    public let protocols: Set<NMPLocalSignerProtocol>
    public let iosDetectionURI: String?
    public let nip46LaunchScheme: String?
    public let androidDetectionURI: String?
    public let androidPackageID: String?
    public let androidProviderAuthority: String?

    init(_ ffi: FfiLocalSignerApp) {
        id = ffi.id
        displayName = ffi.displayName
        protocols = Set(ffi.protocols.map {
            switch $0 {
            case .nip46: .nip46
            case .nip55: .nip55
            }
        })
        iosDetectionURI = ffi.iosDetectionUri
        nip46LaunchScheme = ffi.nip46LaunchScheme
        androidDetectionURI = ffi.androidDetectionUri
        androidPackageID = ffi.androidPackageId
        androidProviderAuthority = ffi.androidProviderAuthority
    }
}

public enum NMPLocalSignerDiscovery {
    public static var known: [NMPLocalSigner] {
        localSignerCatalog().map(NMPLocalSigner.init)
    }

    static func matchingIOSApps(canOpen: (URL) -> Bool) -> [NMPLocalSigner] {
        known.filter { signer in
            guard
                let raw = signer.iosDetectionURI,
                let url = URL(string: raw)
            else { return false }
            return canOpen(url)
        }
    }

    /// Executes only the OS availability probe. Protocol support, selection,
    /// and handoff construction remain Rust-owned catalog facts.
    #if canImport(UIKit)
    @MainActor
    public static func installed() -> [NMPLocalSigner] {
        matchingIOSApps(canOpen: UIApplication.shared.canOpenURL)
    }
    #endif
}

public struct NMPNip46ClientMetadata: Sendable, Hashable {
    public var name: String?
    public var url: String?
    public var image: String?

    public init(name: String? = nil, url: String? = nil, image: String? = nil) {
        self.name = name
        self.url = url
        self.image = image
    }

    func toFfi() -> FfiNip46ClientMetadata {
        FfiNip46ClientMetadata(name: name, url: url, image: image)
    }
}

public enum NMPNip46ConnectionState: Sendable, Equatable {
    case connecting
    case available
    case unavailable
    case relayAuthentication(relay: String)
    case authorizationRequired(url: String)
    /// Protocol handshake completed; the user key is now known.
    case connected(userPublicKey: String)
    /// Stronger than `connected`: the signer is attached to this engine.
    case ready(userPublicKey: String)
    case failed(reason: String)
}

final class NIP46Observer: Nip46ConnectionObserver, @unchecked Sendable {
    let stream: AsyncStream<NMPNip46ConnectionState>
    private let continuation: AsyncStream<NMPNip46ConnectionState>.Continuation

    init() {
        var captured: AsyncStream<NMPNip46ConnectionState>.Continuation!
        stream = AsyncStream(bufferingPolicy: .bufferingNewest(32)) { captured = $0 }
        continuation = captured
    }

    func onEvent(event: FfiNip46ConnectionEvent) {
        let state: NMPNip46ConnectionState = switch event {
        case .connecting: .connecting
        case .available: .available
        case .unavailable: .unavailable
        case .relayAuthentication(let relay): .relayAuthentication(relay: relay)
        case .authorizationRequired(let url): .authorizationRequired(url: url)
        case .connected(let userPublicKey): .connected(userPublicKey: userPublicKey)
        }
        continuation.yield(state)
    }

    func onReady(userPublicKey: String) {
        continuation.yield(.ready(userPublicKey: userPublicKey))
    }

    func onFailed(reason: String) {
        continuation.yield(.failed(reason: reason))
    }

    func onClosed() {
        continuation.finish()
    }
}

public final class NMPNip46Connection: @unchecked Sendable {
    public let states: AsyncStream<NMPNip46ConnectionState>
    fileprivate let observer: NIP46Observer
    private let closeAction: () -> Void
    private let closeLock = NSLock()
    private var isClosed = false

    fileprivate init(observer: NIP46Observer, ffiConnection: Nip46Connection) {
        self.observer = observer
        closeAction = { ffiConnection.disconnect() }
        states = observer.stream
    }

    init(observer: NIP46Observer, closeAction: @escaping () -> Void) {
        self.observer = observer
        self.closeAction = closeAction
        states = observer.stream
    }

    /// Idempotently detach this exact signer session and finish `states`.
    /// Dropping the last connection reference has the same effect.
    public func close() {
        closeLock.lock()
        guard !isClosed else {
            closeLock.unlock()
            return
        }
        isClosed = true
        closeLock.unlock()
        closeAction()
    }

    deinit {
        close()
    }
}

public struct NMPNip46Invitation: Sendable {
    fileprivate let ffi: FfiNip46Invitation

    /// Generic chooser URI, or an app-specific URI such as Primal's
    /// `primalconnect://` when a catalog signer is supplied.
    public func uri(for signer: NMPLocalSigner? = nil) throws -> String {
        try nmpRethrowing { try ffi.uri(signerId: signer?.id) }
    }
}

extension NMPEngine {
    public func nip46Invitation(
        relays: [String],
        permissions: String? = nil,
        metadata: NMPNip46ClientMetadata = .init()
    ) throws -> NMPNip46Invitation {
        let invitation = try nmpRethrowing {
            try ffi.nip46Invitation(
                relays: relays,
                permissions: permissions,
                metadata: metadata.toFfi()
            )
        }
        return NMPNip46Invitation(ffi: invitation)
    }

    @discardableResult
    public func connectNip46(
        bunkerURI: String,
        timeout: Duration = .seconds(60)
    ) throws -> NMPNip46Connection {
        let observer = NIP46Observer()
        let ffiConnection = try nmpRethrowing {
            try ffi.connectNip46Bunker(
                bunkerUri: bunkerURI,
                timeoutMillis: timeout.milliseconds,
                observer: observer
            )
        }
        return NMPNip46Connection(observer: observer, ffiConnection: ffiConnection)
    }

    @discardableResult
    public func connectNip46(
        invitation: NMPNip46Invitation,
        timeout: Duration = .seconds(60)
    ) throws -> NMPNip46Connection {
        let observer = NIP46Observer()
        let ffiConnection = try nmpRethrowing {
            try ffi.connectNip46Invitation(
                invitation: invitation.ffi,
                timeoutMillis: timeout.milliseconds,
                observer: observer
            )
        }
        return NMPNip46Connection(observer: observer, ffiConnection: ffiConnection)
    }

    #if canImport(UIKit)
    /// Start listening before launching the selected signer. A successful
    /// `open` only means the OS accepted the handoff; callers should wait for
    /// `.ready` before treating the signer as connected.
    @MainActor
    public func oneClickConnectNip46(
        signer: NMPLocalSigner,
        relays: [String],
        permissions: String? = nil,
        metadata: NMPNip46ClientMetadata = .init(),
        timeout: Duration = .seconds(60)
    ) async throws -> NMPNip46Connection {
        guard signer.protocols.contains(.nip46) else {
            throw NMPError.invalidSigner("\(signer.displayName) does not support NIP-46")
        }
        let invitation = try nip46Invitation(
            relays: relays,
            permissions: permissions,
            metadata: metadata
        )
        let rawURI = try invitation.uri(for: signer)
        guard let url = URL(string: rawURI) else {
            throw NMPError.invalidSigner("invalid signer handoff URI")
        }
        let connection = try connectNip46(invitation: invitation, timeout: timeout)
        guard await UIApplication.shared.open(url) else {
            connection.observer.onFailed(reason: "the signer app did not accept the handoff")
            connection.close()
            return connection
        }
        return connection
    }
    #endif
}

private extension Duration {
    var milliseconds: UInt64 {
        let components = self.components
        let seconds = UInt64(max(0, components.seconds))
        let millisFromAttoseconds = UInt64(max(0, components.attoseconds)) / 1_000_000_000_000_000
        return seconds.saturatingMultiply(1_000).saturatingAdd(millisFromAttoseconds)
    }
}

private extension UInt64 {
    func saturatingMultiply(_ other: UInt64) -> UInt64 {
        multipliedReportingOverflow(by: other).overflow ? .max : self * other
    }

    func saturatingAdd(_ other: UInt64) -> UInt64 {
        addingReportingOverflow(other).overflow ? .max : self + other
    }
}
