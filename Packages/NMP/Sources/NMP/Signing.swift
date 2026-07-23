import Foundation
import NMPFFI

/// Immutable event body for `NMPEngine.signEvent`. NMP freezes the author
/// from the engine's active account before invoking the signer.
public struct NMPUnsignedEvent: Sendable, Hashable {
    public let createdAt: UInt64
    public let kind: UInt16
    public let tags: [[String]]
    public let content: String

    public init(
        createdAt: UInt64,
        kind: UInt16,
        tags: [[String]],
        content: String
    ) {
        self.createdAt = createdAt
        self.kind = kind
        self.tags = tags
        self.content = content
    }

    func toFfi() -> FfiSignEventRequest {
        FfiSignEventRequest(
            createdAt: createdAt,
            kind: kind,
            tags: tags,
            content: content
        )
    }
}

/// Exact verified result of a sign-only operation. Receiving this value does
/// not imply storage, publication, or creation of a write receipt.
public struct NMPSignedEvent: Sendable, Hashable {
    public let id: String
    public let pubkey: String
    public let createdAt: UInt64
    public let kind: UInt16
    public let tags: [[String]]
    public let content: String
    public let signature: String

    init(_ ffi: FfiSignedEvent) {
        id = ffi.id
        pubkey = ffi.pubkey
        createdAt = ffi.createdAt
        kind = ffi.kind
        tags = ffi.tags
        content = ffi.content
        signature = ffi.sig
    }
}

/// Translate the one-shot sign-only outcome's typed failure (#680). A signer
/// refusal is a typed `NMPError`; an engine-side `.Cancelled` becomes a Swift
/// `CancellationError` so `try await` cancellation reads naturally.
func mapSignEventFailure(_ failure: FfiSignEventFailure) -> Error {
    switch failure {
    case .SignerUnavailable(let reason): return NMPError.signerUnavailable(reason)
    case .SignerRejected(let reason): return NMPError.signerRejected(reason)
    case .InvalidSignerOutput(let reason): return NMPError.invalidSignerOutput(reason)
    case .Cancelled: return CancellationError()
    case .AlreadyConsumed: return NMPError.signEventAlreadyConsumed
    }
}

extension NMPEngine {
    /// Sign one exact event with the active signer without creating a write
    /// intent, pending row, receipt, relay plan, or publication (#680).
    ///
    /// `NmpEngine.signEvent` synchronously returns a one-shot
    /// `NmpSignEventHandle`; awaiting `handle.signed()` delivers the verified
    /// event or a typed failure. Task cancellation is wired through
    /// `withTaskCancellationHandler` to `handle.cancel()` -- MANDATORY because
    /// Swift task cancellation never reaches Rust and never interrupts the
    /// in-flight `await` (#680); the cancel wakes `signed()` to a `.Cancelled`
    /// failure, surfaced here as `CancellationError`.
    public func signEvent(_ event: NMPUnsignedEvent) async throws -> NMPSignedEvent {
        let handle = try nmpRethrowing {
            try ffi.signEvent(event: event.toFfi())
        }
        return try await withTaskCancellationHandler {
            do {
                return NMPSignedEvent(try await handle.signed())
            } catch let failure as FfiSignEventFailure {
                throw mapSignEventFailure(failure)
            } catch let error as FfiError {
                throw NMPError(error)
            }
        } onCancel: {
            handle.cancel()
        }
    }
}
