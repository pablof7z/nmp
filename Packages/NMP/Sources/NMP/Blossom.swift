// The opt-in Blossom (BUD-01/02/04/11/12) blob surface (#555, epic #216
// T15-A-BLOSSOM) -- thin, idiomatic wrappers over the `FfiBlossom*`
// generated types, mirroring NIP29.swift's shape: the draft builders are
// free functions needing no `NMPEngine` instance, and every operation's
// failure taxonomy stays its OWN typed error enum (never collapsed into
// `NMPError`, never a message string).
//
// SIGNING FLOW: nothing here signs. Build a draft, get it signed, validate:
//
//   let draft = try blossomUploadAuthorizationDraft(
//       authorPubkeyHex: currentAccount, blobSha256Hex: hash,
//       createdAt: now, expiration: now + 300, description: "upload")
//   // Engine sign-only path (`signEvent` freezes the author from whatever
//   // account is current when it runs, so validation is handed the draft's
//   // own author and refuses a signature by anyone else):
//   let signed = try await engine.signEvent(draft.signRequest)
//   let auth = try BlossomAuthorization.validate(
//       signedEvent: signed, authorPubkeyHex: draft.authorPubkeyHex,
//       verb: .upload, blobSha256Hex: hash, now: now)
//   // External signers instead sign `draft.unsignedEventJSON` and pass the
//   // signed event's canonical JSON to `BlossomAuthorization.validate(
//   // signedEventJSON:authorPubkeyHex:verb:blobSha256Hex:now:)`.
//
// THREADING: the underlying FFI client methods BLOCK for up to the request
// deadline. `BlossomClient`'s async methods dispatch them onto a global
// queue via a checked continuation (no existing wrapper in this package
// bridges a blocking FFI call yet, so this file sets that precedent
// deliberately off the cooperative pool), keeping the caller's thread and
// Swift concurrency's cooperative threads unblocked.

import Foundation
import NMPFFI

/// The BUD-11 authorization verbs (`FfiBlossomVerb` mirror). `get` has no
/// draft builder yet -- the `get`/`media` endpoints are epic-#216
/// follow-ups.
public enum BlossomVerb: Sendable, Hashable {
    case upload
    case delete
    case get
    case list

    init(_ ffi: FfiBlossomVerb) {
        switch ffi {
        case .upload: self = .upload
        case .delete: self = .delete
        case .get: self = .get
        case .list: self = .list
        }
    }

    func toFfi() -> FfiBlossomVerb {
        switch self {
        case .upload: return .upload
        case .delete: return .delete
        case .get: return .get
        case .list: return .list
        }
    }
}

/// A BUD-02 blob descriptor (`FfiBlobDescriptor` mirror). Returned by
/// `upload`/`mirror` only after the sha256 integrity gate; `list` rows are
/// strictly parsed but remain unverified server claims.
public struct BlobDescriptor: Sendable, Hashable {
    public let url: String
    /// 64 lowercase hex characters -- the strict BUD-01 blob identity.
    public let sha256: String
    public let size: UInt64
    public let mimeType: String?
    public let uploaded: UInt64?

    init(_ ffi: FfiBlobDescriptor) {
        url = ffi.url
        sha256 = ffi.sha256
        size = ffi.size
        mimeType = ffi.mimeType
        uploaded = ffi.uploaded
    }
}

/// Strict lowercase-hex sha256 parse refusals (`FfiBlossomSha256HexError`
/// mirror).
public enum BlossomSha256HexError: Sendable, Hashable {
    case badLength(length: UInt64)
    case notLowercaseHex(character: String)

    init(_ ffi: FfiBlossomSha256HexError) {
        switch ffi {
        case .badLength(let length): self = .badLength(length: length)
        case .notLowercaseHex(let character): self = .notLowercaseHex(character: character)
        }
    }
}

/// Blossom server base-URL admission refusals (`FfiBlossomServerUrlError`
/// mirror). Nothing is ever normalized away; each rule is its own case.
public enum BlossomServerUrlError: Sendable, Hashable {
    case parse(reason: String)
    case missingHost
    case unsupportedScheme(scheme: String)
    case credentialed
    case nonRootPath(path: String)
    case queryOrFragment

    init(_ ffi: FfiBlossomServerUrlError) {
        switch ffi {
        case .parse(let reason): self = .parse(reason: reason)
        case .missingHost: self = .missingHost
        case .unsupportedScheme(let scheme): self = .unsupportedScheme(scheme: scheme)
        case .credentialed: self = .credentialed
        case .nonRootPath(let path): self = .nonRootPath(path: path)
        case .queryOrFragment: self = .queryOrFragment
        }
    }
}

/// Strict BUD-02 descriptor parse refusals (`FfiBlossomDescriptorError`
/// mirror).
public enum BlossomDescriptorError: Sendable, Hashable {
    case tooLarge(limitBytes: UInt64)
    case json(reason: String)
    case missingUrl
    case missingSha256
    case missingSize
    case badSha256(BlossomSha256HexError)

    init(_ ffi: FfiBlossomDescriptorError) {
        switch ffi {
        case .tooLarge(let limitBytes): self = .tooLarge(limitBytes: limitBytes)
        case .json(let reason): self = .json(reason: reason)
        case .missingUrl: self = .missingUrl
        case .missingSha256: self = .missingSha256
        case .missingSize: self = .missingSize
        case .badSha256(let error): self = .badSha256(BlossomSha256HexError(error))
        }
    }
}

/// Draft-construction + validation failures (`FfiBlossomAuthError` mirror)
/// -- every BUD-11 clause a refused authorization failed keeps its own
/// case.
public enum BlossomAuthError: Error, Sendable, Hashable {
    case invalidAuthorPubkey(got: String)
    case invalidBlobSha256(BlossomSha256HexError)
    case invalidEventJson(reason: String)
    case expirationNotAfterCreatedAt(createdAt: UInt64, expiration: UInt64)
    case wrongKind(found: UInt16)
    /// The event was signed by an identity other than the expected author.
    /// The signature is valid, just someone else's, so `.badSignature`
    /// cannot catch it -- this is what refuses an authorization drafted
    /// for one account and signed by whichever account is current.
    case authorMismatch(expectedPubkeyHex: String, foundPubkeyHex: String)
    case badSignature(reason: String)
    case missingVerb
    case multipleVerbs
    case verbMismatch(expected: BlossomVerb, found: String)
    case blobNotBound(expectedSha256Hex: String)
    case missingExpiration
    case expired(expiration: UInt64, now: UInt64)
    case createdAtInFuture(createdAt: UInt64, now: UInt64)

    init(_ ffi: FfiBlossomAuthError) {
        switch ffi {
        case .InvalidAuthorPubkey(let got): self = .invalidAuthorPubkey(got: got)
        case .InvalidBlobSha256(let error): self = .invalidBlobSha256(BlossomSha256HexError(error))
        case .InvalidEventJson(let reason): self = .invalidEventJson(reason: reason)
        case .ExpirationNotAfterCreatedAt(let createdAtSecs, let expirationSecs):
            self = .expirationNotAfterCreatedAt(createdAt: createdAtSecs, expiration: expirationSecs)
        case .WrongKind(let found): self = .wrongKind(found: found)
        case .AuthorMismatch(let expectedPubkeyHex, let foundPubkeyHex):
            self = .authorMismatch(
                expectedPubkeyHex: expectedPubkeyHex, foundPubkeyHex: foundPubkeyHex)
        case .BadSignature(let reason): self = .badSignature(reason: reason)
        case .MissingVerb: self = .missingVerb
        case .MultipleVerbs: self = .multipleVerbs
        case .VerbMismatch(let expected, let found):
            self = .verbMismatch(expected: BlossomVerb(expected), found: found)
        case .BlobNotBound(let expectedSha256Hex):
            self = .blobNotBound(expectedSha256Hex: expectedSha256Hex)
        case .MissingExpiration: self = .missingExpiration
        case .Expired(let expirationSecs, let nowSecs):
            self = .expired(expiration: expirationSecs, now: nowSecs)
        case .CreatedAtInFuture(let createdAtSecs, let nowSecs):
            self = .createdAtInFuture(createdAt: createdAtSecs, now: nowSecs)
        }
    }
}

/// `BlossomClient.upload`'s exhaustive failure taxonomy
/// (`FfiBlossomUploadError` mirror) -- never collapsed with the other
/// operations'.
public enum BlossomUploadError: Error, Sendable, Hashable {
    case invalidServerUrl(BlossomServerUrlError)
    case runtimeUnavailable(reason: String)
    case clientBuild(reason: String)
    case authorizationBlobMismatch(
        expectedSha256Hex: String,
        authorizedVerb: BlossomVerb,
        authorizedBlobSha256Hex: String?
    )
    case network(detail: String)
    case redirectRefused(status: UInt16)
    case authRejected(status: UInt16, reason: String?)
    case serverRejected(status: UInt16, reason: String?)
    case serverError(status: UInt16, reason: String?)
    case responseTooLarge(limitBytes: UInt64)
    case descriptorInvalid(BlossomDescriptorError)
    case sha256Mismatch(expectedSha256Hex: String, returnedSha256Hex: String)

    init(_ ffi: FfiBlossomUploadError) {
        switch ffi {
        case .InvalidServerUrl(let error): self = .invalidServerUrl(BlossomServerUrlError(error))
        case .RuntimeUnavailable(let reason): self = .runtimeUnavailable(reason: reason)
        case .ClientBuild(let reason): self = .clientBuild(reason: reason)
        case .AuthorizationBlobMismatch(
            let expectedSha256Hex, let authorizedVerb, let authorizedBlobSha256Hex
        ):
            self = .authorizationBlobMismatch(
                expectedSha256Hex: expectedSha256Hex,
                authorizedVerb: BlossomVerb(authorizedVerb),
                authorizedBlobSha256Hex: authorizedBlobSha256Hex
            )
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
        }
    }
}

/// `BlossomClient.mirror`'s exhaustive failure taxonomy
/// (`FfiBlossomMirrorError` mirror) -- the server's 409 hash refusal and
/// 502 origin-fetch failure keep their own cases, distinct from the
/// client-side `sha256Mismatch` integrity gate.
public enum BlossomMirrorError: Error, Sendable, Hashable {
    case invalidServerUrl(BlossomServerUrlError)
    case invalidExpectedSha256(BlossomSha256HexError)
    case runtimeUnavailable(reason: String)
    case clientBuild(reason: String)
    case authorizationBlobMismatch(
        expectedSha256Hex: String,
        authorizedVerb: BlossomVerb,
        authorizedBlobSha256Hex: String?
    )
    case network(detail: String)
    case redirectRefused(status: UInt16)
    case authRejected(status: UInt16, reason: String?)
    case hashMismatchRefused(reason: String?)
    case originFetchFailed(reason: String?)
    case serverRejected(status: UInt16, reason: String?)
    case serverError(status: UInt16, reason: String?)
    case responseTooLarge(limitBytes: UInt64)
    case descriptorInvalid(BlossomDescriptorError)
    case sha256Mismatch(expectedSha256Hex: String, returnedSha256Hex: String)

    init(_ ffi: FfiBlossomMirrorError) {
        switch ffi {
        case .InvalidServerUrl(let error): self = .invalidServerUrl(BlossomServerUrlError(error))
        case .InvalidExpectedSha256(let error):
            self = .invalidExpectedSha256(BlossomSha256HexError(error))
        case .RuntimeUnavailable(let reason): self = .runtimeUnavailable(reason: reason)
        case .ClientBuild(let reason): self = .clientBuild(reason: reason)
        case .AuthorizationBlobMismatch(
            let expectedSha256Hex, let authorizedVerb, let authorizedBlobSha256Hex
        ):
            self = .authorizationBlobMismatch(
                expectedSha256Hex: expectedSha256Hex,
                authorizedVerb: BlossomVerb(authorizedVerb),
                authorizedBlobSha256Hex: authorizedBlobSha256Hex
            )
        case .Network(let detail): self = .network(detail: detail)
        case .RedirectRefused(let status): self = .redirectRefused(status: status)
        case .AuthRejected(let status, let reason):
            self = .authRejected(status: status, reason: reason)
        case .HashMismatchRefused(let reason): self = .hashMismatchRefused(reason: reason)
        case .OriginFetchFailed(let reason): self = .originFetchFailed(reason: reason)
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
        }
    }
}

/// `BlossomClient.delete`'s exhaustive failure taxonomy
/// (`FfiBlossomDeleteError` mirror) -- 404 keeps its own `notFound` case
/// ("already gone" is actionable for idempotent callers).
public enum BlossomDeleteError: Error, Sendable, Hashable {
    case invalidServerUrl(BlossomServerUrlError)
    case invalidBlobSha256(BlossomSha256HexError)
    case runtimeUnavailable(reason: String)
    case clientBuild(reason: String)
    case authorizationBlobMismatch(
        expectedSha256Hex: String,
        authorizedVerb: BlossomVerb,
        authorizedBlobSha256Hex: String?
    )
    case network(detail: String)
    case redirectRefused(status: UInt16)
    case authRejected(status: UInt16, reason: String?)
    case notFound(reason: String?)
    case serverRejected(status: UInt16, reason: String?)
    case serverError(status: UInt16, reason: String?)

    init(_ ffi: FfiBlossomDeleteError) {
        switch ffi {
        case .InvalidServerUrl(let error): self = .invalidServerUrl(BlossomServerUrlError(error))
        case .InvalidBlobSha256(let error): self = .invalidBlobSha256(BlossomSha256HexError(error))
        case .RuntimeUnavailable(let reason): self = .runtimeUnavailable(reason: reason)
        case .ClientBuild(let reason): self = .clientBuild(reason: reason)
        case .AuthorizationBlobMismatch(
            let expectedSha256Hex, let authorizedVerb, let authorizedBlobSha256Hex
        ):
            self = .authorizationBlobMismatch(
                expectedSha256Hex: expectedSha256Hex,
                authorizedVerb: BlossomVerb(authorizedVerb),
                authorizedBlobSha256Hex: authorizedBlobSha256Hex
            )
        case .Network(let detail): self = .network(detail: detail)
        case .RedirectRefused(let status): self = .redirectRefused(status: status)
        case .AuthRejected(let status, let reason):
            self = .authRejected(status: status, reason: reason)
        case .NotFound(let reason): self = .notFound(reason: reason)
        case .ServerRejected(let status, let reason):
            self = .serverRejected(status: status, reason: reason)
        case .ServerError(let status, let reason):
            self = .serverError(status: status, reason: reason)
        }
    }
}

/// `BlossomClient.list`'s exhaustive failure taxonomy
/// (`FfiBlossomListError` mirror) -- one malformed row fails the whole
/// call typed, never a silently shortened success.
public enum BlossomListError: Error, Sendable, Hashable {
    case invalidServerUrl(BlossomServerUrlError)
    case invalidOwnerPubkey(got: String)
    case invalidCursor(BlossomSha256HexError)
    case runtimeUnavailable(reason: String)
    case clientBuild(reason: String)
    case wrongVerb(authorizedVerb: BlossomVerb)
    case network(detail: String)
    case redirectRefused(status: UInt16)
    case authRejected(status: UInt16, reason: String?)
    case serverRejected(status: UInt16, reason: String?)
    case serverError(status: UInt16, reason: String?)
    case responseTooLarge(limitBytes: UInt64)
    case bodyNotAnArray(reason: String)
    case invalidDescriptor(index: UInt64, error: BlossomDescriptorError)

    init(_ ffi: FfiBlossomListError) {
        switch ffi {
        case .InvalidServerUrl(let error): self = .invalidServerUrl(BlossomServerUrlError(error))
        case .InvalidOwnerPubkey(let got): self = .invalidOwnerPubkey(got: got)
        case .InvalidCursor(let error): self = .invalidCursor(BlossomSha256HexError(error))
        case .RuntimeUnavailable(let reason): self = .runtimeUnavailable(reason: reason)
        case .ClientBuild(let reason): self = .clientBuild(reason: reason)
        case .WrongVerb(let authorizedVerb):
            self = .wrongVerb(authorizedVerb: BlossomVerb(authorizedVerb))
        case .Network(let detail): self = .network(detail: detail)
        case .RedirectRefused(let status): self = .redirectRefused(status: status)
        case .AuthRejected(let status, let reason):
            self = .authRejected(status: status, reason: reason)
        case .ServerRejected(let status, let reason):
            self = .serverRejected(status: status, reason: reason)
        case .ServerError(let status, let reason):
            self = .serverError(status: status, reason: reason)
        case .ResponseTooLarge(let limitBytes): self = .responseTooLarge(limitBytes: limitBytes)
        case .BodyNotAnArray(let reason): self = .bodyNotAnArray(reason: reason)
        case .InvalidDescriptor(let index, let error):
            self = .invalidDescriptor(index: index, error: BlossomDescriptorError(error))
        }
    }
}

/// `NMPEngine.uploadBlossom`'s signing-stage failure (`FfiSignEventFailure`
/// mirror, the sign-only cases only) -- its own typed case rather than
/// folded into `signEvent`'s `NMPError`/`CancellationError` idiom, because
/// this is one of EIGHT failure domains `UploadBlossomError` keeps
/// separate, not the whole call's only failure mode.
public enum UploadBlossomSignError: Sendable, Hashable {
    case signerUnavailable(reason: String)
    case signerRejected(reason: String)
    case invalidSignerOutput(reason: String)
    case cancelled

    init(_ ffi: FfiSignEventFailure) {
        switch ffi {
        case .SignerUnavailable(let reason): self = .signerUnavailable(reason: reason)
        case .SignerRejected(let reason): self = .signerRejected(reason: reason)
        case .InvalidSignerOutput(let reason): self = .invalidSignerOutput(reason: reason)
        case .Cancelled, .AlreadyConsumed:
            // `AlreadyConsumed` cannot reach this boundary: `uploadBlossom`
            // awaits the one-shot signer result exactly once, internally,
            // and never exposes the handle a second await could race.
            self = .cancelled
        }
    }
}

/// `NMPEngine.uploadBlossom`'s exhaustive failure taxonomy
/// (`FfiUploadBlossomError` mirror) -- each stage keeps its own real
/// upstream shape, reused wholesale from the types above, exactly as this
/// file's other operations never collapse their taxonomies into one another.
public enum UploadBlossomError: Error, Sendable, Hashable {
    case signedOut
    case engineClosed
    case emptyContentType
    case authorization(BlossomAuthError)
    case sign(UploadBlossomSignError)
    case invalidServerUrl(BlossomServerUrlError)
    case clientBuild(reason: String)
    case upload(BlossomUploadError)

    init(_ ffi: FfiUploadBlossomError) {
        switch ffi {
        case .SignedOut: self = .signedOut
        case .EngineClosed: self = .engineClosed
        case .EmptyContentType: self = .emptyContentType
        case .Authorization(let error): self = .authorization(BlossomAuthError(error))
        case .Sign(let error): self = .sign(UploadBlossomSignError(error))
        case .InvalidServerUrl(let error): self = .invalidServerUrl(BlossomServerUrlError(error))
        case .ClientBuild(let reason): self = .clientBuild(reason: reason)
        case .Upload(let error): self = .upload(BlossomUploadError(error))
        }
    }
}

extension NMPEngine {
    /// Upload `blob` to `serverURL` end to end: hash the exact bytes, build
    /// and sign a BUD-11 `upload` authorization through the current
    /// account's registered signer, validate it, and perform the real
    /// `PUT /upload` -- all as one call (#971). No author, timestamp,
    /// expiration, draft, sign request, or authorization crosses this
    /// boundary; NMP owns every one of those internally.
    ///
    /// `contentType` becomes the imeta `m` a later NIP-68 composition step
    /// requires and must be non-empty. `description` is the human-readable
    /// BUD-11 authorization reason shown to the server operator, not the
    /// eventual post text.
    ///
    /// Returns the SAME verified-descriptor vocabulary
    /// `BlossomClient.upload` already returns: its `sha256` was proven
    /// equal to the hash of exactly the bytes uploaded.
    public func uploadBlossom(
        serverURL: String,
        blob: Data,
        contentType: String,
        description: String
    ) async throws -> BlobDescriptor {
        do {
            return BlobDescriptor(
                try await ffi.uploadBlossom(
                    serverUrl: serverURL,
                    blob: blob,
                    contentType: contentType,
                    description: description
                )
            )
        } catch let error as FfiUploadBlossomError {
            throw UploadBlossomError(error)
        }
    }
}

/// An UNSIGNED kind:24242 authorization draft (`FfiBlossomAuthDraft`
/// mirror). Sign it via the engine (`signRequest` ->
/// `NMPEngine.signEvent`) or hand `unsignedEventJSON` to an external
/// signer; nothing in this SDK holds keys. Either signer decides the
/// author, so pass `authorPubkeyHex` back to `BlossomAuthorization.
/// validate`: a signature by any other identity is refused as
/// `.authorMismatch`.
public struct BlossomAuthorizationDraft: Sendable, Hashable {
    /// The identity this draft was composed for. Hand it to
    /// `BlossomAuthorization.validate` -- that is what binds the signature
    /// to the identity the caller meant.
    public let authorPubkeyHex: String
    /// The draft as canonical unsigned-event JSON, for external signers.
    public let unsignedEventJSON: String
    /// The blob this draft binds via its `x` tag (`nil` for `list`).
    public let blobSha256Hex: String?
    /// The verb this draft grants.
    public let verb: BlossomVerb
    public let createdAt: UInt64
    public let kind: UInt16
    public let tags: [[String]]
    public let content: String

    init(_ ffi: FfiBlossomAuthDraft) {
        authorPubkeyHex = ffi.authorPubkeyHex
        unsignedEventJSON = ffi.unsignedEventJson
        blobSha256Hex = ffi.blobSha256Hex
        verb = BlossomVerb(ffi.verb)
        createdAt = ffi.createdAtSecs
        kind = ffi.kind
        tags = ffi.tags
        content = ffi.content
    }

    /// The engine sign-only request for this exact draft. `NMPEngine.
    /// signEvent` freezes the author from whichever account is current
    /// when it runs; validation compares that against `authorPubkeyHex`.
    public var signRequest: NMPUnsignedEvent {
        NMPUnsignedEvent(createdAt: createdAt, kind: kind, tags: tags, content: content)
    }
}

private func blossomAuthRethrowing<T>(_ body: () throws -> T) throws -> T {
    do {
        return try body()
    } catch let error as FfiBlossomAuthError {
        throw BlossomAuthError(error)
    }
}

/// Compose an UNSIGNED BUD-11 `upload` authorization draft (kind 24242).
/// BUD-04 NOTE: a mirror is authorized with THIS builder -- the spec
/// assigns mirroring the `upload` verb. Free function, no engine needed.
public func blossomUploadAuthorizationDraft(
    authorPubkeyHex: String,
    blobSha256Hex: String,
    createdAt: UInt64,
    expiration: UInt64,
    description: String
) throws -> BlossomAuthorizationDraft {
    try BlossomAuthorizationDraft(
        blossomAuthRethrowing {
            try NMPFFI.blossomUploadAuthorizationDraft(
                authorPubkeyHex: authorPubkeyHex,
                blobSha256Hex: blobSha256Hex,
                createdAtSecs: createdAt,
                expirationSecs: expiration,
                description: description
            )
        }
    )
}

/// Compose an UNSIGNED BUD-12 `delete` authorization draft. Exactly ONE
/// blob is bound (BUD-12 forbids multi-blob deletes via extra `x` tags).
public func blossomDeleteAuthorizationDraft(
    authorPubkeyHex: String,
    blobSha256Hex: String,
    createdAt: UInt64,
    expiration: UInt64,
    description: String
) throws -> BlossomAuthorizationDraft {
    try BlossomAuthorizationDraft(
        blossomAuthRethrowing {
            try NMPFFI.blossomDeleteAuthorizationDraft(
                authorPubkeyHex: authorPubkeyHex,
                blobSha256Hex: blobSha256Hex,
                createdAtSecs: createdAt,
                expirationSecs: expiration,
                description: description
            )
        }
    )
}

/// Compose an UNSIGNED BUD-12 `list` authorization draft. No `x` tag:
/// listing is scoped to a pubkey by the request path, not to any blob.
public func blossomListAuthorizationDraft(
    authorPubkeyHex: String,
    createdAt: UInt64,
    expiration: UInt64,
    description: String
) throws -> BlossomAuthorizationDraft {
    try BlossomAuthorizationDraft(
        blossomAuthRethrowing {
            try NMPFFI.blossomListAuthorizationDraft(
                authorPubkeyHex: authorPubkeyHex,
                createdAtSecs: createdAt,
                expirationSecs: expiration,
                description: description
            )
        }
    )
}

/// A signed kind:24242 event PROVEN (at construction) to satisfy every
/// BUD-11 check (`FfiBlossomAuthorization` mirror) -- the only value
/// `BlossomClient`'s operations accept, so an unvalidated event can never
/// become an `Authorization` header.
public final class BlossomAuthorization: @unchecked Sendable {
    let ffi: FfiBlossomAuthorization

    private init(_ ffi: FfiBlossomAuthorization) {
        self.ffi = ffi
    }

    /// Fail-closed BUD-11 validation of a signed event supplied as
    /// canonical event JSON (the external-signer path). `authorPubkeyHex`
    /// is the identity the authorization must speak for -- pass
    /// `BlossomAuthorizationDraft.authorPubkeyHex`; a signature by any
    /// other account is `.authorMismatch`. `verb` is what the
    /// caller is ABOUT to use the authorization for; `blobSha256Hex` binds
    /// the exact blob for verbs that grant one (`upload`/`delete`; mirror
    /// validates under `.upload`); `now` is the caller's clock (unix
    /// seconds).
    public static func validate(
        signedEventJSON: String,
        authorPubkeyHex: String,
        verb: BlossomVerb,
        blobSha256Hex: String?,
        now: UInt64
    ) throws -> BlossomAuthorization {
        try BlossomAuthorization(
            blossomAuthRethrowing {
                try FfiBlossomAuthorization.validate(
                    signedEventJson: signedEventJSON,
                    authorPubkeyHex: authorPubkeyHex,
                    verb: verb.toFfi(),
                    blobSha256Hex: blobSha256Hex,
                    nowSecs: now
                )
            }
        )
    }

    /// Fail-closed BUD-11 validation of the exact value
    /// `NMPEngine.signEvent` returns (the engine sign-only path) -- the
    /// same checks as `validate(signedEventJSON:...)`. `signEvent` signs
    /// with whichever account the engine currently holds, which need not
    /// be the one the draft was built for; that divergence surfaces here
    /// as `.authorMismatch`.
    public static func validate(
        signedEvent: NMPSignedEvent,
        authorPubkeyHex: String,
        verb: BlossomVerb,
        blobSha256Hex: String?,
        now: UInt64
    ) throws -> BlossomAuthorization {
        let event = FfiSignedEvent(
            id: signedEvent.id,
            pubkey: signedEvent.pubkey,
            createdAt: signedEvent.createdAt,
            kind: signedEvent.kind,
            tags: signedEvent.tags,
            content: signedEvent.content,
            sig: signedEvent.signature
        )
        return try BlossomAuthorization(
            blossomAuthRethrowing {
                try FfiBlossomAuthorization.validateSignedEvent(
                    event: event,
                    authorPubkeyHex: authorPubkeyHex,
                    verb: verb.toFfi(),
                    blobSha256Hex: blobSha256Hex,
                    nowSecs: now
                )
            }
        )
    }

    /// The verb this authorization was validated FOR.
    public var verb: BlossomVerb {
        BlossomVerb(ffi.verb())
    }

    /// The blob hash this authorization was proven to bind (`nil` for
    /// verbs validated without a blob binding).
    public var blobSha256Hex: String? {
        ffi.blobSha256Hex()
    }
}

/// `BlossomClient` construction knobs (`FfiBlossomClientConfig` mirror).
/// `nil` means the Rust crate's default.
public struct BlossomClientConfig: Sendable, Hashable {
    /// Cap on a single-descriptor response body (upload/mirror).
    public var maxResponseBytes: UInt64?
    /// Cap on a `GET /list` response body.
    public var maxListResponseBytes: UInt64?
    /// Overall request deadline (connect, headers, and body), seconds.
    public var requestDeadlineSeconds: UInt64?

    public init(
        maxResponseBytes: UInt64? = nil,
        maxListResponseBytes: UInt64? = nil,
        requestDeadlineSeconds: UInt64? = nil
    ) {
        self.maxResponseBytes = maxResponseBytes
        self.maxListResponseBytes = maxListResponseBytes
        self.requestDeadlineSeconds = requestDeadlineSeconds
    }

    func toFfi() -> FfiBlossomClientConfig {
        FfiBlossomClientConfig(
            maxResponseBytes: maxResponseBytes,
            maxListResponseBytes: maxListResponseBytes,
            requestDeadlineSecs: requestDeadlineSeconds
        )
    }
}

/// Run one blocking Blossom FFI call on a global queue, keeping both the
/// caller and Swift concurrency's cooperative pool unblocked.
private func blossomBlocking<T: Sendable>(
    _ body: @escaping @Sendable () throws -> T
) async throws -> T {
    try await withCheckedThrowingContinuation { continuation in
        DispatchQueue.global(qos: .userInitiated).async {
            continuation.resume(with: Result(catching: body))
        }
    }
}

/// The BUD-02/04/12 blob client (`FfiBlossomClient` mirror). Every method
/// is `async` and dispatches the underlying BLOCKING FFI call off the
/// caller's thread; each failure arrives as that operation's own typed
/// error enum.
public final class BlossomClient: @unchecked Sendable {
    let ffi: FfiBlossomClient

    public init(config: BlossomClientConfig = BlossomClientConfig()) {
        ffi = FfiBlossomClient(config: config.toFfi())
    }

    /// `PUT /upload` of `blob`'s exact bytes -- self-verifying end to end:
    /// the returned descriptor's sha256 was PROVEN equal to the hash of
    /// the uploaded bytes. `authorization` must be an `upload` grant bound
    /// to exactly those bytes.
    public func upload(
        serverURL: String,
        blob: Data,
        contentType: String? = nil,
        authorization: BlossomAuthorization
    ) async throws -> BlobDescriptor {
        let ffi = self.ffi
        let auth = authorization.ffi
        return try await blossomBlocking {
            do {
                return BlobDescriptor(
                    try ffi.upload(
                        serverUrl: serverURL, blob: blob, contentType: contentType, auth: auth
                    )
                )
            } catch let error as FfiBlossomUploadError {
                throw BlossomUploadError(error)
            }
        }
    }

    /// `PUT /mirror` (BUD-04): ask `serverURL` to download the blob at
    /// `sourceURL` itself, integrity-gated against `expectedSha256Hex`.
    /// `authorization` is an `upload` grant bound to that hash.
    public func mirror(
        serverURL: String,
        sourceURL: String,
        expectedSha256Hex: String,
        authorization: BlossomAuthorization
    ) async throws -> BlobDescriptor {
        let ffi = self.ffi
        let auth = authorization.ffi
        return try await blossomBlocking {
            do {
                return BlobDescriptor(
                    try ffi.mirror(
                        serverUrl: serverURL,
                        sourceUrl: sourceURL,
                        expectedSha256Hex: expectedSha256Hex,
                        auth: auth
                    )
                )
            } catch let error as FfiBlossomMirrorError {
                throw BlossomMirrorError(error)
            }
        }
    }

    /// `DELETE /<sha256>` (BUD-12). `authorization` is a `delete` grant
    /// bound to EXACTLY `blobSha256Hex`; 404 surfaces as
    /// `BlossomDeleteError.notFound` ("already gone").
    public func delete(
        serverURL: String,
        blobSha256Hex: String,
        authorization: BlossomAuthorization
    ) async throws {
        let ffi = self.ffi
        let auth = authorization.ffi
        try await blossomBlocking {
            do {
                try ffi.delete(serverUrl: serverURL, blobSha256Hex: blobSha256Hex, auth: auth)
            } catch let error as FfiBlossomDeleteError {
                throw BlossomDeleteError(error)
            }
        }
    }

    /// `GET /list/<pubkey>` (BUD-12): the blobs `serverURL` stores for
    /// `ownerPubkeyHex`, newest first. `authorization` is optional -- a
    /// server that requires a `list` grant answers 401, surfaced as
    /// `BlossomListError.authRejected`. `cursorSha256Hex`/`limit` are the
    /// BUD-12 pagination parameters, sent only when set.
    public func list(
        serverURL: String,
        ownerPubkeyHex: String,
        cursorSha256Hex: String? = nil,
        limit: UInt32? = nil,
        authorization: BlossomAuthorization? = nil
    ) async throws -> [BlobDescriptor] {
        let ffi = self.ffi
        let auth = authorization?.ffi
        return try await blossomBlocking {
            do {
                return try ffi.list(
                    serverUrl: serverURL,
                    ownerPubkeyHex: ownerPubkeyHex,
                    cursorSha256Hex: cursorSha256Hex,
                    limit: limit,
                    auth: auth
                ).map(BlobDescriptor.init)
            } catch let error as FfiBlossomListError {
                throw BlossomListError(error)
            }
        }
    }
}
