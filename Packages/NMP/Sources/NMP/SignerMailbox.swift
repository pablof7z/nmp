// The app-supplied signer door (#1238).
//
// Before this file existed, a Swift app could register no signer at all: the
// only identity it could give NMP was a raw secret key handed to
// `addAccount`, because `Engine::add_signer` is generic over a Rust trait and
// cannot cross UniFFI. Anything else an app might sign with -- a Secure
// Enclave key, a Keychain-backed key it will not surrender, a remote bunker,
// a hardware device -- had nowhere to attach.
//
// The door is a stream, not a callback. NMP does not call into the app
// (#783): it enqueues immutable requests and returns, and the app drains them
// on its own executor. That is what makes a signer that takes ten seconds
// because a person is looking at a confirmation screen an ordinary case
// rather than a stalled engine.

import Foundation
import NMPFFI

/// One unsigned event NMP needs a signature for.
///
/// `pubkey` is frozen by the write that asked for it: sign as that key or
/// refuse. Returning a signature by any other key fails the write at NMP's
/// promotion boundary rather than publishing something unintended.
public struct NMPSignatureRequestBody: Sendable, Hashable {
    public let pubkey: String
    public let createdAt: UInt64
    public let kind: UInt16
    public let tags: [[String]]
    public let content: String

    init(_ ffi: FfiUnsignedEvent) {
        pubkey = ffi.pubkey
        createdAt = ffi.createdAt
        kind = ffi.kind
        tags = ffi.tags
        content = ffi.content
    }
}

/// The closed set of refusals an app can give for one signature request.
///
/// Deliberately narrower than NMP's own signer failure vocabulary: a timeout
/// or a disconnect is something NMP concludes about a signer, not something a
/// signer says about itself.
public enum NMPSignerRejection: Sendable, Equatable {
    /// The person said no. Terminal for the write -- retrying cannot change a
    /// decision somebody already made.
    case rejected(reason: String)
    /// The signer cannot answer right now: device locked, bunker offline, app
    /// backgrounded. Retryable, and the write parks and waits.
    case unavailable

    var ffi: FfiSignerRejection {
        switch self {
        case .rejected(let reason): .rejected(reason: reason)
        case .unavailable: .unavailable
        }
    }
}

/// Why settling a signature request did not take effect.
public enum NMPSignatureSettleError: Error, Sendable, Equatable {
    /// The request was cancelled, or the write that asked for it went away.
    /// The answer is discarded; the mailbox is unaffected.
    case noLongerAwaited
    /// This request was already settled. Each request carries exactly one
    /// answer.
    case alreadySettled
    /// `id`, `pubkey` or `sig` was not the fixed-width hex the protocol
    /// defines. The request is NOT spent -- correct the value and settle again.
    case malformedSignedEvent(reason: String)

    init(_ ffi: FfiSignatureSettleError) {
        switch ffi {
        case .NoLongerAwaited: self = .noLongerAwaited
        case .AlreadySettled: self = .alreadySettled
        case .MalformedSignedEvent(let reason): self = .malformedSignedEvent(reason: reason)
        }
    }
}

/// One signature NMP is waiting for.
///
/// Settles exactly once, and the object enforces it: a second `resolve` or
/// `reject` throws `.alreadySettled` rather than delivering a second answer.
/// Letting it deinit without settling is a legal answer too -- NMP hears the
/// ordinary retryable unavailable and the write parks, which is exactly what
/// an app whose signer went away should say.
public final class NMPSignatureRequest: @unchecked Sendable {
    /// The exact body to sign.
    public let body: NMPSignatureRequestBody

    private let ffi: NmpSignatureRequest

    init(_ ffi: NmpSignatureRequest) {
        self.ffi = ffi
        body = NMPSignatureRequestBody(ffi.unsignedEvent())
    }

    /// Answer with a signature over exactly `body`.
    public func resolve(_ signed: NMPSignedEvent) throws {
        do {
            try ffi.resolve(event: signed.toFfi())
        } catch let error as FfiSignatureSettleError {
            throw NMPSignatureSettleError(error)
        }
    }

    /// Answer with a refusal.
    public func reject(_ reason: NMPSignerRejection) throws {
        do {
            try ffi.reject(reason: reason.ffi)
        } catch let error as FfiSignatureSettleError {
            throw NMPSignatureSettleError(error)
        }
    }
}

/// The app's end of one registered signer: an async sequence of signature
/// requests, and the exact-instance proof that removes the registration.
///
/// Drain it from a long-lived task:
///
/// ```swift
/// let mailbox = try await engine.addSigner(publicKey: pubkey)
/// Task {
///     for try await request in mailbox.requests {
///         do {
///             try request.resolve(await myHardwareSigner.sign(request.body))
///         } catch {
///             try? request.reject(.unavailable)
///         }
///     }
/// }
/// ```
///
/// Exactly one drainer at a time: two concurrent `next()` calls would each
/// believe they held the only copy of a take-once answer, so the second is
/// refused rather than silently losing a request.
public final class NMPSignerMailbox: @unchecked Sendable {
    /// The key this mailbox signs for.
    public let publicKey: String

    let ffi: NmpSignerMailbox

    init(ffi: NmpSignerMailbox) {
        self.ffi = ffi
        publicKey = ffi.publicKey()
    }

    /// Await the next signature request, or `nil` once the mailbox is closed
    /// and drained.
    public func next() async throws -> NMPSignatureRequest? {
        let request = try await nmpRethrowingAsync { try await ffi.next() }
        return request.map(NMPSignatureRequest.init)
    }

    /// The requests as an async sequence, for `for try await`.
    public var requests: NMPSignatureRequestSequence {
        NMPSignatureRequestSequence(mailbox: self)
    }

    /// Stop accepting requests and end a parked `next()`.
    ///
    /// This does NOT remove the registration: writes for this key then park on
    /// an unavailable signer, exactly as they do before any signer attaches.
    /// `NMPEngine.removeSigner(_:)` removes it.
    public func cancel() {
        ffi.cancel()
    }
}

/// `for try await request in mailbox.requests`.
public struct NMPSignatureRequestSequence: AsyncSequence {
    public typealias Element = NMPSignatureRequest

    let mailbox: NMPSignerMailbox

    public struct AsyncIterator: AsyncIteratorProtocol {
        let mailbox: NMPSignerMailbox

        public mutating func next() async throws -> NMPSignatureRequest? {
            try await mailbox.next()
        }
    }

    public func makeAsyncIterator() -> AsyncIterator {
        AsyncIterator(mailbox: mailbox)
    }
}

extension NMPSignedEvent {
    func toFfi() -> FfiSignedEvent {
        FfiSignedEvent(
            id: id,
            pubkey: pubkey,
            createdAt: createdAt,
            kind: kind,
            tags: tags,
            content: content,
            sig: signature
        )
    }
}

extension NMPEngine {
    /// Register a signing capability this app owns, for exactly `publicKey`.
    ///
    /// `addAccount(secretKey:)` is the door for a key NMP holds; it takes the
    /// secret. This one takes no secret -- only the public key the app can
    /// sign for -- and returns the mailbox of requests to drain. Registering
    /// does not make the key active; use `setActiveAccount(_:)` for that.
    ///
    /// Registering the same key again replaces the capability and invalidates
    /// the previous mailbox's registration.
    public func addSigner(publicKey: String) throws -> NMPSignerMailbox {
        let mailbox = try nmpRethrowing {
            try ffi.addSignerMailbox(publicKey: publicKey)
        }
        return NMPSignerMailbox(ffi: mailbox)
    }

    /// Remove only the signer installation proven by `mailbox`. Repeated or
    /// stale removal returns `false` and can never detach a replacement
    /// registered for the same key.
    @discardableResult
    public func removeSigner(_ mailbox: NMPSignerMailbox) throws -> Bool {
        try nmpRethrowing {
            try ffi.removeSignerMailbox(mailbox: mailbox.ffi)
        }
    }
}
