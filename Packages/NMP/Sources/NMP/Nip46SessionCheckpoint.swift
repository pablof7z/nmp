import Foundation
import NMPFFI

/// Distinguishes a NIP-46 session paired via `nostrconnect://` from one
/// dialed via `bunker://` (#571). Restore mechanics are identical either
/// way -- kept because "absence of a reusable client checkpoint is
/// observable rather than guessed from partial metadata" is a hard
/// requirement, not because restore branches on it.
public enum NMPNip46SessionOrigin: Sendable, Equatable {
    case clientInitiated
    case bunker

    init(_ ffi: FfiNip46Origin) {
        switch ffi {
        case .clientInitiated: self = .clientInitiated
        case .bunker: self = .bunker
        }
    }

    func toFfi() -> FfiNip46Origin {
        switch self {
        case .clientInitiated: .clientInitiated
        case .bunker: .bunker
        }
    }
}

/// NMP-owned checkpoint of the minimum secrets and descriptor needed to
/// reconnect an already-authorized NIP-46 client session without another
/// pairing handshake (#571) -- read from `NMPNip46Connection.checkpoint()`
/// once `.ready`, persisted by an `NMPNip46SessionCheckpointStore`, and
/// consumed by `NMPEngine.restoreNip46Session`/`importNip46Session`.
///
/// `clientSecretKey` hands the raw secret to whoever holds this value, by
/// the same design as `NMPLocalAccountCheckpoint` (see that protocol's
/// doc): the app owns its keys, and the recommended secure checkpoint
/// stores keep it in the platform vault. Nothing here ever logs, prints, or
/// otherwise surfaces it -- `description`/`debugDescription` are redacted.
public struct NMPNip46SessionCheckpoint: Sendable, Equatable {
    public let clientSecretKey: String
    public let userPublicKey: String
    public let remoteSignerPublicKey: String
    public let relays: [String]
    public let origin: NMPNip46SessionOrigin

    public init(
        clientSecretKey: String,
        userPublicKey: String,
        remoteSignerPublicKey: String,
        relays: [String],
        origin: NMPNip46SessionOrigin
    ) {
        self.clientSecretKey = clientSecretKey
        self.userPublicKey = userPublicKey
        self.remoteSignerPublicKey = remoteSignerPublicKey
        self.relays = relays
        self.origin = origin
    }

    init(_ ffi: FfiNip46SessionCheckpoint) {
        clientSecretKey = ffi.clientSecretKey
        userPublicKey = ffi.userPublicKey
        remoteSignerPublicKey = ffi.remoteSignerPublicKey
        relays = ffi.relays
        origin = NMPNip46SessionOrigin(ffi.origin)
    }

    func toFfi() -> FfiNip46SessionCheckpoint {
        FfiNip46SessionCheckpoint(
            clientSecretKey: clientSecretKey,
            userPublicKey: userPublicKey,
            remoteSignerPublicKey: remoteSignerPublicKey,
            relays: relays,
            origin: origin.toFfi()
        )
    }
}

extension NMPNip46SessionCheckpoint: CustomStringConvertible, CustomDebugStringConvertible {
    public var description: String {
        "NMPNip46SessionCheckpoint(userPublicKey: \(userPublicKey), " +
            "remoteSignerPublicKey: \(remoteSignerPublicKey), relays: \(relays), " +
            "origin: \(origin), clientSecretKey: [redacted])"
    }

    public var debugDescription: String { description }
}

/// NMP-owned versioned wire serialization of `NMPNip46SessionCheckpoint`,
/// independent of the FFI-generated type's own field order/renames (#571).
extension NMPNip46SessionCheckpoint {
    private struct WireV1: Codable {
        var version: Int
        var clientSecretKey: String
        var userPublicKey: String
        var remoteSignerPublicKey: String
        var relays: [String]
        var origin: String
    }

    /// A serialized checkpoint blob did not decode as this format -- never
    /// carries the raw bytes, so this is always safe to log.
    public enum SerializationError: Error, Equatable {
        case unsupportedVersion(Int)
        case invalidOrigin(String)
        case malformed
    }

    /// Encode this checkpoint as NMP's own versioned wire format. A
    /// checkpoint store persists this `Data` verbatim.
    public func serialize() throws -> Data {
        let wire = WireV1(
            version: 1,
            clientSecretKey: clientSecretKey,
            userPublicKey: userPublicKey,
            remoteSignerPublicKey: remoteSignerPublicKey,
            relays: relays,
            origin: origin == .clientInitiated ? "clientInitiated" : "bunker"
        )
        return try JSONEncoder().encode(wire)
    }

    /// Decode a checkpoint previously produced by `serialize()`. An
    /// unrecognized version, corrupt bytes, or an unknown origin tag fails
    /// closed with a typed `SerializationError` rather than guessing.
    public static func deserialize(_ data: Data) throws -> NMPNip46SessionCheckpoint {
        let wire: WireV1
        do {
            wire = try JSONDecoder().decode(WireV1.self, from: data)
        } catch {
            throw SerializationError.malformed
        }
        guard wire.version == 1 else {
            throw SerializationError.unsupportedVersion(wire.version)
        }
        let origin: NMPNip46SessionOrigin
        switch wire.origin {
        case "clientInitiated": origin = .clientInitiated
        case "bunker": origin = .bunker
        default: throw SerializationError.invalidOrigin(wire.origin)
        }
        return NMPNip46SessionCheckpoint(
            clientSecretKey: wire.clientSecretKey,
            userPublicKey: wire.userPublicKey,
            remoteSignerPublicKey: wire.remoteSignerPublicKey,
            relays: wire.relays,
            origin: origin
        )
    }
}

/// The seam an `NMPEngine` NIP-46 restore/import door reads/writes through,
/// mirroring `NMPLocalAccountCheckpoint`'s load/save/clear shape (#571): a
/// platform-vault provider, an app-custom store, or an app-supplied
/// convenience type are all drop-ins. A checkpoint hands the raw client
/// secret to its holder by the same design as `NMPLocalAccountCheckpoint`
/// (see that protocol's doc) -- the app owns its keys.
public protocol NMPNip46SessionCheckpointStore: Sendable {
    /// The checkpointed session, or `nil` when none is stored.
    func loadCheckpoint() throws -> NMPNip46SessionCheckpoint?
    /// Persist `checkpoint`, replacing any previous one.
    func saveCheckpoint(_ checkpoint: NMPNip46SessionCheckpoint) throws
    /// Remove the checkpoint. Idempotent -- a later `loadCheckpoint` returns
    /// `nil`. Never called by `close()`/`deinit`; only explicit session
    /// removal clears it.
    func clear() throws
}

/// The raw generated FFI record (`NMPFFI.FfiNip46SessionCheckpoint`) has no
/// redacted `Debug` of its own -- unlike `nmp-signer`'s Rust type it
/// mirrors, a plain Swift struct's default reflection-based description
/// prints every stored property, including `clientSecretKey`. This
/// retroactive conformance closes that gap for the one boundary moment the
/// raw FFI value exists (inside `NMPNip46SessionCheckpoint.init(_:)` and
/// `NMPNip46Connection.checkpoint()`) before it is wrapped into the redacted
/// `NMPNip46SessionCheckpoint` apps actually hold.
extension FfiNip46SessionCheckpoint: CustomStringConvertible, CustomDebugStringConvertible {
    public var description: String {
        "FfiNip46SessionCheckpoint(userPublicKey: \(userPublicKey), " +
            "remoteSignerPublicKey: \(remoteSignerPublicKey), relays: \(relays), " +
            "origin: \(origin), clientSecretKey: [redacted])"
    }

    public var debugDescription: String { description }
}
