import Foundation
import NMPFFI

/// The one-shot engine-authorized Blossom upload (#971).
///
/// `Blossom.swift` next door is the LOW-LEVEL projection: the app builds a
/// kind:24242 draft, signs it, validates it, and drives `BlossomClient`
/// itself. This file is the opposite bargain, and the one an app should
/// normally use: state what you are uploading and where, and NMP owns the
/// author, the clock, the sha256, the BUD-11 composition, the signature, the
/// re-validation and the request.
///
/// Nothing in this file accepts or returns an author pubkey, event kind, tag,
/// unsigned event, sign request, signed authorization, caller timestamp,
/// expiration, blob hash, raw `NMPFFI` value, or a callback that participates
/// in the decision path -- which is the whole point. The result is the same
/// `BlobDescriptor` the low-level upload returns; there is no second
/// verified-asset type.

/// The engine-authorized upload's exhaustive failure taxonomy
/// (`FfiBlossomUploadFailure` mirror).
///
/// Deliberately DISTINCT from `BlossomUploadError`, which belongs to the
/// low-level client: that operation cannot fail for a signer, clock or
/// active-account reason, and this one cannot fail for an authorization the
/// caller supplied. Nothing is flattened into a message string.
///
/// `.cancelled` has no case here on purpose -- see `uploadBlossom`: a
/// withdrawn upload surfaces as Swift's own `CancellationError`.
public enum BlossomUploadFailure: Error, Sendable, Hashable {
    case invalidServerUrl(BlossomServerUrlError)
    case emptyContentType
    /// NMP could not compose a representable BUD-11 window at this instant.
    case authorizationWindow(createdAt: UInt64, lifetimeSeconds: UInt64)
    case noActiveSigner
    case signerUnavailable(reason: String)
    case signerRejected(reason: String)
    case invalidSignerOutput(reason: String)
    case authorizationExpired(expiration: UInt64, now: UInt64)
    /// The engine clock moved backwards between composing the authorization
    /// and validating it. A clock fact, not a signer fault.
    case clockMovedBackward(createdAt: UInt64, now: UInt64)
    case clientBuild(reason: String)
    case localHostNotAdmitted(host: String)
    case network(detail: String)
    case redirectRefused(status: UInt16)
    case authRejected(status: UInt16, reason: String?)
    case serverRejected(status: UInt16, reason: String?)
    case serverError(status: UInt16, reason: String?)
    case responseTooLarge(limitBytes: UInt64)
    case descriptorInvalid(BlossomDescriptorError)
    case sha256Mismatch(expectedSha256Hex: String, returnedSha256Hex: String)
    case engineClosed
    /// The one-shot result was already delivered to a prior await.
    case alreadyConsumed

    init?(_ ffi: FfiBlossomUploadFailure) {
        switch ffi {
        case .InvalidServerUrl(let error): self = .invalidServerUrl(BlossomServerUrlError(error))
        case .EmptyContentType: self = .emptyContentType
        case .AuthorizationWindow(let createdAtSecs, let lifetimeSecs):
            self = .authorizationWindow(createdAt: createdAtSecs, lifetimeSeconds: lifetimeSecs)
        case .NoActiveSigner: self = .noActiveSigner
        case .SignerUnavailable(let reason): self = .signerUnavailable(reason: reason)
        case .SignerRejected(let reason): self = .signerRejected(reason: reason)
        case .InvalidSignerOutput(let reason): self = .invalidSignerOutput(reason: reason)
        case .AuthorizationExpired(let expirationSecs, let nowSecs):
            self = .authorizationExpired(expiration: expirationSecs, now: nowSecs)
        case .ClockMovedBackward(let createdAtSecs, let nowSecs):
            self = .clockMovedBackward(createdAt: createdAtSecs, now: nowSecs)
        case .ClientBuild(let reason): self = .clientBuild(reason: reason)
        case .LocalHostNotAdmitted(let host): self = .localHostNotAdmitted(host: host)
        case .Network(let detail): self = .network(detail: detail)
        case .RedirectRefused(let status): self = .redirectRefused(status: status)
        case .AuthRejected(let status, let reason):
            self = .authRejected(status: status, reason: reason)
        case .ServerRejected(let status, let reason):
            self = .serverRejected(status: status, reason: reason)
        case .ServerError(let status, let reason):
            self = .serverError(status: status, reason: reason)
        case .ResponseTooLarge(let limitBytes): self = .responseTooLarge(limitBytes: limitBytes)
        case .DescriptorInvalid(let error): self = .descriptorInvalid(BlossomDescriptorError(error))
        case .Sha256Mismatch(let expectedSha256Hex, let returnedSha256Hex):
            self = .sha256Mismatch(
                expectedSha256Hex: expectedSha256Hex,
                returnedSha256Hex: returnedSha256Hex
            )
        case .EngineClosed: self = .engineClosed
        case .AlreadyConsumed: self = .alreadyConsumed
        // Withdrawal is not a Blossom fault: it becomes `CancellationError`.
        case .Cancelled: return nil
        }
    }
}

/// Translate the engine-authorized upload's typed failure. `.Cancelled`
/// becomes a Swift `CancellationError` so `try await` cancellation reads
/// naturally, exactly as `mapSignEventFailure` does for the sign-only door.
func mapBlossomUploadFailure(_ failure: FfiBlossomUploadFailure) -> Error {
    BlossomUploadFailure(failure) ?? CancellationError()
}

extension NMPEngine {
    /// Upload one blob to a Blossom server, authorized by the active signer.
    ///
    /// NMP owns the entire transaction inside this one call: it freezes the
    /// author from the active account, reads its own clock, hashes these exact
    /// bytes once, composes and signs the BUD-11 kind:24242 authorization,
    /// re-validates the signature against that exact hash before any HTTP, and
    /// performs the hardened `PUT /upload`. The returned descriptor's sha256
    /// has been PROVEN equal to the hash of the bytes that were sent.
    ///
    /// Task cancellation is wired through `withTaskCancellationHandler` to
    /// `handle.cancel()` -- MANDATORY, because Swift task cancellation never
    /// reaches Rust on its own. Cancelling before the request is transmitted
    /// means no HTTP happened at all. Cancelling AFTER it was transmitted is an
    /// observation gap: the local operation stopped, and NMP does not claim
    /// whether the server stored the bytes. Both surface as `CancellationError`.
    ///
    /// Nothing about this is durable: there is no receipt, no retry owner and
    /// no stored row. Whether to try again is the product's decision.
    public func uploadBlossom(
        serverURL: String,
        blob: Data,
        contentType: String,
        description: String
    ) async throws -> BlobDescriptor {
        let handle: NmpBlossomUploadHandle
        do {
            handle = try ffi.uploadBlossom(
                request: FfiBlossomUploadRequest(
                    serverUrl: serverURL,
                    blob: blob,
                    contentType: contentType,
                    description: description
                )
            )
        } catch let failure as FfiBlossomUploadFailure {
            throw mapBlossomUploadFailure(failure)
        }
        return try await withTaskCancellationHandler {
            do {
                return BlobDescriptor(try await handle.uploaded())
            } catch let failure as FfiBlossomUploadFailure {
                throw mapBlossomUploadFailure(failure)
            }
        } onCancel: {
            handle.cancel()
        }
    }
}
