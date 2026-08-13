import Foundation
import NMPFFI

/// One validated decoded Nostr public key.
public struct NMPPublicKey: @unchecked Sendable, Equatable {
    let ffi: FfiPublicKey

    public init(bytes: Data) throws {
        ffi = try nmpRethrowing { try FfiPublicKey.fromBytes(bytes: bytes) }
    }

    init(ffi: FfiPublicKey) {
        self.ffi = ffi
    }

    public var bytes: Data { ffi.bytes() }

    public static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.bytes == rhs.bytes
    }
}

/// One validated decoded Nostr private key.
///
/// The Swift wrapper provides no byte accessor and renders only a redacted
/// description. NMP wipes the native buffer it owns when released; it makes
/// no claim about the caller's original `Data` or FFI transport copies.
public final class NMPPrivateKey: @unchecked Sendable, CustomStringConvertible,
    CustomDebugStringConvertible
{
    let ffi: FfiPrivateKey

    public init(bytes: Data) throws {
        ffi = try nmpRethrowing { try FfiPrivateKey.fromBytes(bytes: bytes) }
    }

    private init(ffi: FfiPrivateKey) {
        self.ffi = ffi
    }

    public static func generate() -> NMPPrivateKey {
        NMPPrivateKey(ffi: FfiPrivateKey.generate())
    }

    public var description: String { "NMPPrivateKey(<redacted>)" }
    public var debugDescription: String { description }
}

/// Opaque bytes representing one complete engine session.
///
/// The bytes may be stored and restored as a whole, but their account and
/// signer-provider material is owned and interpreted only by NMP. Treat them
/// as sensitive. String interpolation and debug output are always redacted.
public final class NMPSessionPayload: @unchecked Sendable, CustomStringConvertible,
    CustomDebugStringConvertible
{
    let ffi: FfiSessionPayload

    public init(bytes: Data) {
        ffi = FfiSessionPayload.fromBytes(bytes: bytes)
    }

    init(ffi: FfiSessionPayload) {
        self.ffi = ffi
    }

    /// The opaque transport bytes. Store and restore this value atomically;
    /// do not parse or partially modify it.
    public var bytes: Data { ffi.bytes() }

    public var description: String { "NMPSessionPayload(<redacted>)" }
    public var debugDescription: String { description }
}

/// The persistable signer-provider kind configured for an account.
/// Availability is deliberately not inferred from this value.
public enum NMPSessionProviderKind: Sendable, Equatable {
    case localKey

    init(_ ffi: FfiSessionProviderKind) {
        switch ffi {
        case .localKey: self = .localKey
        }
    }
}

/// The current operational state of one cryptographic capability.
public enum NMPCapabilityAvailability: Sendable, Equatable {
    case unsupported
    case available
    case unavailable(reason: String)

    init(_ ffi: FfiCapabilityAvailability) {
        switch ffi {
        case .unsupported: self = .unsupported
        case .available: self = .available
        case .unavailable(let reason): self = .unavailable(reason: reason)
        }
    }
}

/// One account in the engine's current session.
///
/// Signer-backed and public-key-only accounts use this same handle. Removing
/// the handle removes the whole account, including its provider. Account
/// identity is its public key; re-adding a provider updates that account.
public final class NMPSessionAccount: @unchecked Sendable {
    public let publicKey: NMPPublicKey
    public let providerKind: NMPSessionProviderKind?
    public let signingAvailability: NMPCapabilityAvailability
    let ffi: FfiSessionAccount

    init(ffi: FfiSessionAccount) {
        self.ffi = ffi
        publicKey = NMPPublicKey(ffi: ffi.publicKey())
        providerKind = ffi.provider().map(NMPSessionProviderKind.init)
        signingAvailability = NMPCapabilityAvailability(ffi.signingAvailability())
    }
}

/// The engine-owned view of one complete account session.
///
/// This object does not own another runtime or lifecycle. It projects session
/// mutations through its `NMPEngine` and never retains secret material itself.
public final class NMPSession: @unchecked Sendable {
    private let ffi: NmpEngineProtocol

    init(ffi: NmpEngineProtocol) {
        self.ffi = ffi
    }

    public var accounts: [NMPSessionAccount] {
        get throws {
            try nmpRethrowing {
                try ffi.session().accounts.map(NMPSessionAccount.init)
            }
        }
    }

    public var current: NMPSessionAccount? {
        get throws {
            try nmpRethrowing {
                let snapshot = try ffi.session()
                guard let currentPublicKey = snapshot.currentPublicKey else {
                    return nil
                }
                return snapshot.accounts
                    .first { $0.publicKey().bytes() == currentPublicKey.bytes() }
                    .map(NMPSessionAccount.init)
            }
        }
    }

    /// Add a private-key-backed account. When requested, selection happens in
    /// the same engine transition as addition.
    public func add(
        privateKey: NMPPrivateKey,
        makeCurrent: Bool = false
    ) throws -> NMPSessionAccount {
        try nmpRethrowing {
            NMPSessionAccount(
                ffi: try ffi.addPrivateKeyAccount(
                    privateKey: privateKey.ffi,
                    makeCurrent: makeCurrent
                )
            )
        }
    }

    /// Add an ordinary account without a signer provider. Decode bech32 at
    /// the app's human input boundary before calling this method.
    public func add(
        publicKey: NMPPublicKey,
        makeCurrent: Bool = false
    ) throws -> NMPSessionAccount {
        try nmpRethrowing {
            NMPSessionAccount(
                ffi: try ffi.addPublicKeyAccount(
                    publicKey: publicKey.ffi,
                    makeCurrent: makeCurrent
                )
            )
        }
    }

    public func makeCurrent(_ account: NMPSessionAccount) throws {
        try nmpRethrowing {
            try ffi.makeCurrentAccount(account: account.ffi)
        }
    }

    /// Remove the whole account at this public key. Repeated removal is a no-op.
    @discardableResult
    public func remove(_ account: NMPSessionAccount) throws -> Bool {
        try nmpRethrowing {
            try ffi.removeAccount(account: account.ffi)
        }
    }

    /// Clear accounts, providers, and current selection without clearing
    /// cached events, receipts, or accepted write obligations.
    public func clear() throws {
        try nmpRethrowing { try ffi.clearSession() }
    }

    /// Export the one complete opaque value suitable for atomic app storage.
    public func export() throws -> NMPSessionPayload {
        try nmpRethrowing {
            NMPSessionPayload(ffi: try ffi.exportSession())
        }
    }
}
