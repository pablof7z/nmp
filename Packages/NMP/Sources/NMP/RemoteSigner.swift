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

/// `nmp_signer::BunkerParseError` mirror (mirrors `nmp-ffi`'s own
/// `FfiBunkerParseError`; see that type's doc for the Rust side of each
/// case).
public enum NMPBunkerParseFailure: Sendable, Equatable {
    case empty
    case tooLong(len: UInt64)
    case wrongScheme
    case missingRemoteSignerKey
    case invalidRemoteSignerKey
    case missingRelay
    case tooManyRelays(count: UInt64)
    case invalidRelay(String)
    case malformed(String)

    init(_ ffi: FfiBunkerParseError) {
        switch ffi {
        case .empty: self = .empty
        case .tooLong(let len): self = .tooLong(len: len)
        case .wrongScheme: self = .wrongScheme
        case .missingRemoteSignerKey: self = .missingRemoteSignerKey
        case .invalidRemoteSignerKey: self = .invalidRemoteSignerKey
        case .missingRelay: self = .missingRelay
        case .tooManyRelays(let count): self = .tooManyRelays(count: count)
        case .invalidRelay(let relay): self = .invalidRelay(relay)
        case .malformed(let reason): self = .malformed(reason)
        }
    }
}

/// Typed NIP-46 connection failure (mirrors `nmp-ffi`'s own
/// `FfiNip46Failure`; see that type's doc for the Rust side of each case).
public enum NMPNip46Failure: Sendable, Equatable {
    case invalidBunkerUri(NMPBunkerParseFailure)
    case missingRelay
    case tooManyRelays(count: UInt64)
    case invitationTooLong(len: UInt64)
    case invalidLaunchScheme(String)
    case timeout
    case disconnected
    case rejected(String)
    case invalidResponse(String)
    case threadUnavailable(component: String, reason: String)
    case executorSaturated(component: String, capacity: UInt64)
    case signerMissingPublicKey
    /// A restore/import's live answer did not match the checkpoint's
    /// expected identity (#571). No signer was attached under the wrong
    /// pubkey.
    case restoredIdentityMismatch(expected: String, actual: String)

    init(_ ffi: FfiNip46Failure) {
        switch ffi {
        case .invalidBunkerUri(let source): self = .invalidBunkerUri(NMPBunkerParseFailure(source))
        case .missingRelay: self = .missingRelay
        case .tooManyRelays(let count): self = .tooManyRelays(count: count)
        case .invitationTooLong(let len): self = .invitationTooLong(len: len)
        case .invalidLaunchScheme(let scheme): self = .invalidLaunchScheme(scheme)
        case .timeout: self = .timeout
        case .disconnected: self = .disconnected
        case .rejected(let reason): self = .rejected(reason)
        case .invalidResponse(let reason): self = .invalidResponse(reason)
        case .threadUnavailable(let component, let reason):
            self = .threadUnavailable(component: component, reason: reason)
        case .executorSaturated(let component, let capacity):
            self = .executorSaturated(component: component, capacity: capacity)
        case .signerMissingPublicKey: self = .signerMissingPublicKey
        case .restoredIdentityMismatch(let expected, let actual):
            self = .restoredIdentityMismatch(expected: expected, actual: actual)
        }
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
    case failed(NMPNip46Failure)
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

    func onFailed(failure: FfiNip46Failure) {
        continuation.yield(.failed(NMPNip46Failure(failure)))
    }

    func onClosed() {
        continuation.finish()
    }
}

public final class NMPNip46Connection: @unchecked Sendable {
    public let states: AsyncStream<NMPNip46ConnectionState>
    fileprivate let observer: NIP46Observer
    /// `nil` only for the test-only `closeAction`-only initializer below;
    /// every connection produced by pairing/restore/import carries this.
    private let ffiConnection: Nip46Connection?
    private let closeAction: () -> Void
    private let closeLock = NSLock()
    private var isClosed = false

    fileprivate init(observer: NIP46Observer, ffiConnection: Nip46Connection) {
        self.observer = observer
        self.ffiConnection = ffiConnection
        closeAction = { ffiConnection.disconnect() }
        states = observer.stream
    }

    init(observer: NIP46Observer, closeAction: @escaping () -> Void) {
        self.observer = observer
        ffiConnection = nil
        self.closeAction = closeAction
        states = observer.stream
    }

    /// Read out this session's checkpoint (#571): the minimum secrets and
    /// descriptor needed to reconnect without another pairing handshake --
    /// see `NMPNip46SessionCheckpoint`'s doc. Refused with a typed error
    /// before this connection has reached `.ready`; checkpointing a session
    /// that never authenticated would persist meaningless material.
    public func checkpoint() throws -> NMPNip46SessionCheckpoint {
        guard let ffiConnection else {
            throw NMPError.invalidSigner("no underlying NIP-46 connection to checkpoint")
        }
        let ffi = try nmpRethrowing { try ffiConnection.checkpoint() }
        return NMPNip46SessionCheckpoint(ffi)
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

    /// Restore an already-authorized NIP-46 client session from `store`'s
    /// checkpoint (#571) -- reconnects the SAME client transport identity to
    /// the SAME remote signer with NO re-pairing handshake. Returns `nil`
    /// without connecting anything when `store` holds no checkpoint. As with
    /// `connectNip46`, `.ready(userPublicKey:)` fires only once the
    /// checkpoint's expected identity is validated against a live answer and
    /// the signer is attached to this engine; a mismatch/corrupt/unavailable
    /// outcome surfaces as a typed `.failed` state, never a thrown error from
    /// this call.
    @discardableResult
    public func restoreNip46Session(
        from store: any NMPNip46SessionCheckpointStore,
        timeout: Duration = .seconds(60)
    ) throws -> NMPNip46Connection? {
        guard let checkpoint = try store.loadCheckpoint() else {
            return nil
        }
        return try restoreNip46Session(checkpoint, timeout: timeout)
    }

    /// Reconnect from an explicit checkpoint value with no store involved --
    /// the primitive `restoreNip46Session(from:)` builds on directly.
    @discardableResult
    public func restoreNip46Session(
        _ checkpoint: NMPNip46SessionCheckpoint,
        timeout: Duration = .seconds(60)
    ) throws -> NMPNip46Connection {
        let observer = NIP46Observer()
        let ffiConnection = try nmpRethrowing {
            try ffi.restoreNip46Session(
                checkpoint: checkpoint.toFfi(),
                timeoutMillis: timeout.milliseconds,
                observer: observer
            )
        }
        return NMPNip46Connection(observer: observer, ffiConnection: ffiConnection)
    }

    /// Brownfield migration door (#571): import a pre-NMP legacy client
    /// session (for example Pod0's Keychain-persisted `nostrconnect://`
    /// material) directly from its raw parts, without first constructing an
    /// NMP-owned checkpoint or ever writing one to a store. Validates
    /// `expectedUserPublicKey` before `.ready` exactly like
    /// `restoreNip46Session`, and never deletes or overwrites the caller's
    /// legacy material -- a mismatch/corrupt import surfaces only as a typed
    /// `.failed` connection state, never by touching `clientSecretKey`'s
    /// original source.
    @discardableResult
    public func importNip46Session(
        clientSecretKey: String,
        expectedUserPublicKey: String,
        remoteSignerPublicKey: String,
        relays: [String],
        origin: NMPNip46SessionOrigin,
        timeout: Duration = .seconds(60)
    ) throws -> NMPNip46Connection {
        let parts = NMPNip46SessionCheckpoint(
            clientSecretKey: clientSecretKey,
            userPublicKey: expectedUserPublicKey,
            remoteSignerPublicKey: remoteSignerPublicKey,
            relays: relays,
            origin: origin
        )
        let observer = NIP46Observer()
        let ffiConnection = try nmpRethrowing {
            try ffi.nip46SessionFromParts(
                parts: parts.toFfi(),
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
            // The OS declined the handoff before any relay-side connection
            // fact could occur; `.disconnected` ("NIP-46 connection ended")
            // is the closest existing `Nip46Error` discriminant -- there is
            // no dedicated variant for a native deep-link refusal since it
            // never reaches Rust's `Nip46Error` taxonomy at all (#494).
            connection.observer.onFailed(failure: .disconnected)
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
