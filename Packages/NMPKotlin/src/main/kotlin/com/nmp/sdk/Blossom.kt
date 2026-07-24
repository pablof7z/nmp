// The opt-in Blossom (BUD-01/02/03/04/11/12) blob surface (#555/#731,
// epic #216
// T15-A-BLOSSOM) -- thin wrappers over the `FfiBlossom*` generated types,
// mirroring Blossom.swift exactly: draft builders as free functions needing
// no `NMPEngine` instance, and every operation's failure taxonomy as its
// OWN typed sealed error class (never collapsed into `NMPError`, never a
// message string).
//
// SIGNING FLOW: nothing here signs. Build a draft, get it signed, validate:
//
//   val draft = blossomUploadAuthorizationDraft(
//       authorPubkeyHex = activeAccount, blobSha256Hex = hash,
//       createdAt = now, expiration = now + 300u, description = "upload")
//   // Engine sign-only path (the author is frozen from the ACTIVE
//   // ACCOUNT, so `authorPubkeyHex` must be that account's pubkey):
//   val signed = engine.signEvent(draft.signRequest)
//   val auth = BlossomAuthorization.validate(
//       signedEvent = signed, verb = BlossomVerb.UPLOAD,
//       blobSha256Hex = hash, now = now)
//   // External signers instead sign `draft.unsignedEventJson` and pass
//   // the signed event's canonical JSON to
//   // `BlossomAuthorization.validate(signedEventJson = ..., ...)`.
//
// THREADING: the underlying FFI client methods BLOCK for up to the request
// deadline. `BlossomClient`'s suspend methods run them inside
// `withContext(Dispatchers.IO)`, keeping the caller's dispatcher unblocked.

package com.nmp.sdk

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import uniffi.nmp_ffi.FfiBlobDescriptor
import uniffi.nmp_ffi.FfiBlossomAuthDraft
import uniffi.nmp_ffi.FfiBlossomAuthException
import uniffi.nmp_ffi.FfiBlossomAuthorization
import uniffi.nmp_ffi.FfiBlossomClient
import uniffi.nmp_ffi.FfiBlossomClientConfig
import uniffi.nmp_ffi.FfiBlossomDeleteException
import uniffi.nmp_ffi.FfiBlossomDescriptorError
import uniffi.nmp_ffi.FfiBlossomListException
import uniffi.nmp_ffi.FfiBlossomMirrorException
import uniffi.nmp_ffi.FfiBlossomMalformedServerEntry
import uniffi.nmp_ffi.FfiBlossomQualificationException
import uniffi.nmp_ffi.FfiBlossomServerAdmission
import uniffi.nmp_ffi.FfiBlossomServerCandidateEvidence
import uniffi.nmp_ffi.FfiBlossomServerCandidatePolicy
import uniffi.nmp_ffi.FfiBlossomServerCandidateSource
import uniffi.nmp_ffi.FfiBlossomServerList
import uniffi.nmp_ffi.FfiBlossomServerListEntryError
import uniffi.nmp_ffi.FfiBlossomServerUrlError
import uniffi.nmp_ffi.FfiBlossomSha256HexError
import uniffi.nmp_ffi.FfiBlossomUploadException
import uniffi.nmp_ffi.FfiBlossomVerb
import uniffi.nmp_ffi.FfiSignedEvent
import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.blossomDeleteAuthorizationDraft as ffiBlossomDeleteAuthorizationDraft
import uniffi.nmp_ffi.blossomListAuthorizationDraft as ffiBlossomListAuthorizationDraft
import uniffi.nmp_ffi.blossomServerListDemand as ffiBlossomServerListDemand
import uniffi.nmp_ffi.blossomUploadAuthorizationDraft as ffiBlossomUploadAuthorizationDraft
import uniffi.nmp_ffi.decodeBlossomServerList as ffiDecodeBlossomServerList

/** The BUD-11 authorization verbs (`FfiBlossomVerb` mirror). `GET` has no
 * draft builder yet -- the `get`/`media` endpoints are epic-#216
 * follow-ups. */
enum class BlossomVerb {
    UPLOAD,
    DELETE,
    GET,
    LIST,
    ;

    internal fun toFfi(): FfiBlossomVerb =
        when (this) {
            UPLOAD -> FfiBlossomVerb.UPLOAD
            DELETE -> FfiBlossomVerb.DELETE
            GET -> FfiBlossomVerb.GET
            LIST -> FfiBlossomVerb.LIST
        }

    companion object {
        internal fun from(ffi: FfiBlossomVerb): BlossomVerb =
            when (ffi) {
                FfiBlossomVerb.UPLOAD -> UPLOAD
                FfiBlossomVerb.DELETE -> DELETE
                FfiBlossomVerb.GET -> GET
                FfiBlossomVerb.LIST -> LIST
            }
    }
}

/** A BUD-02 blob descriptor (`FfiBlobDescriptor` mirror). Returned by
 * `upload`/`mirror` only after the sha256 integrity gate; `list` rows are
 * strictly parsed but remain unverified server claims. */
data class BlobDescriptor(
    val url: String,
    /** 64 lowercase hex characters -- the strict BUD-01 blob identity. */
    val sha256: String,
    val size: ULong,
    val mimeType: String?,
    val uploaded: ULong?,
) {
    companion object {
        internal fun from(ffi: FfiBlobDescriptor): BlobDescriptor =
            BlobDescriptor(ffi.url, ffi.sha256, ffi.size, ffi.mimeType, ffi.uploaded)
    }
}

/** Strict lowercase-hex sha256 parse refusals (`FfiBlossomSha256HexError`
 * mirror). */
sealed class BlossomSha256HexError {
    data class BadLength(val length: ULong) : BlossomSha256HexError()

    data class NotLowercaseHex(val character: String) : BlossomSha256HexError()

    companion object {
        internal fun from(ffi: FfiBlossomSha256HexError): BlossomSha256HexError =
            when (ffi) {
                is FfiBlossomSha256HexError.BadLength -> BadLength(ffi.length)
                is FfiBlossomSha256HexError.NotLowercaseHex -> NotLowercaseHex(ffi.character)
            }
    }
}

/** Blossom server base-URL admission refusals (`FfiBlossomServerUrlError`
 * mirror). Nothing is ever normalized away; each rule is its own case. */
sealed class BlossomServerUrlError {
    data class Parse(val reason: String) : BlossomServerUrlError()

    object MissingHost : BlossomServerUrlError()

    data class UnsupportedScheme(val scheme: String) : BlossomServerUrlError()

    object Credentialed : BlossomServerUrlError()

    data class NonRootPath(val path: String) : BlossomServerUrlError()

    object QueryOrFragment : BlossomServerUrlError()

    internal fun toFfi(): FfiBlossomServerUrlError =
        when (this) {
            is Parse -> FfiBlossomServerUrlError.Parse(reason)
            MissingHost -> FfiBlossomServerUrlError.MissingHost
            is UnsupportedScheme -> FfiBlossomServerUrlError.UnsupportedScheme(scheme)
            Credentialed -> FfiBlossomServerUrlError.Credentialed
            is NonRootPath -> FfiBlossomServerUrlError.NonRootPath(path)
            QueryOrFragment -> FfiBlossomServerUrlError.QueryOrFragment
        }

    companion object {
        internal fun from(ffi: FfiBlossomServerUrlError): BlossomServerUrlError =
            when (ffi) {
                is FfiBlossomServerUrlError.Parse -> Parse(ffi.reason)
                is FfiBlossomServerUrlError.MissingHost -> MissingHost
                is FfiBlossomServerUrlError.UnsupportedScheme -> UnsupportedScheme(ffi.scheme)
                is FfiBlossomServerUrlError.Credentialed -> Credentialed
                is FfiBlossomServerUrlError.NonRootPath -> NonRootPath(ffi.path)
                is FfiBlossomServerUrlError.QueryOrFragment -> QueryOrFragment
            }
    }
}

/** Why one signed BUD-03 `server` tag could not become a typed endpoint. */
sealed class BlossomServerListEntryError {
    object MissingUrl : BlossomServerListEntryError()

    data class InvalidUrl(val error: BlossomServerUrlError) : BlossomServerListEntryError()

    internal fun toFfi(): FfiBlossomServerListEntryError =
        when (this) {
            MissingUrl -> FfiBlossomServerListEntryError.MissingUrl
            is InvalidUrl -> FfiBlossomServerListEntryError.InvalidUrl(error.toFfi())
        }

    companion object {
        internal fun from(ffi: FfiBlossomServerListEntryError): BlossomServerListEntryError =
            when (ffi) {
                is FfiBlossomServerListEntryError.MissingUrl -> MissingUrl
                is FfiBlossomServerListEntryError.InvalidUrl ->
                    InvalidUrl(BlossomServerUrlError.from(ffi.error))
            }
    }
}

/** Position-preserving malformed BUD-03 tag evidence. */
data class BlossomMalformedServerEntry(
    val tagIndex: ULong,
    val rawUrl: String?,
    val error: BlossomServerListEntryError,
) {
    internal fun toFfi(): FfiBlossomMalformedServerEntry =
        FfiBlossomMalformedServerEntry(tagIndex, rawUrl, error.toFfi())

    companion object {
        internal fun from(ffi: FfiBlossomMalformedServerEntry): BlossomMalformedServerEntry =
            BlossomMalformedServerEntry(
                ffi.tagIndex,
                ffi.rawUrl,
                BlossomServerListEntryError.from(ffi.error),
            )
    }
}

/** Closed decode of one canonical signed BUD-03 kind:10063 row. */
data class BlossomServerList(
    val eventId: String,
    val authorPubkey: String,
    /** Canonical URLs in exact signed-list order. */
    val servers: List<String>,
    val malformedEntries: List<BlossomMalformedServerEntry>,
    val serverTagCount: ULong,
    val hasUnexpectedContent: Boolean,
    val isSpecCompliant: Boolean,
) {
    internal fun toFfi(): FfiBlossomServerList =
        FfiBlossomServerList(
            eventId,
            authorPubkey,
            servers,
            malformedEntries.map { it.toFfi() },
            serverTagCount,
            hasUnexpectedContent,
            isSpecCompliant,
        )

    companion object {
        internal fun from(ffi: FfiBlossomServerList): BlossomServerList =
            BlossomServerList(
                ffi.eventId,
                ffi.authorPubkey,
                ffi.servers,
                ffi.malformedEntries.map { BlossomMalformedServerEntry.from(it) },
                ffi.serverTagCount,
                ffi.unexpectedContent,
                ffi.specCompliant,
            )
    }
}

/** Observe the active account's BUD-03 replacement winner through the
 * ordinary live-query model. Signed-out state resolves to zero rows. */
fun blossomServerListDemand(): NMPDemand = NMPDemand.from(ffiBlossomServerListDemand())

/** Decode one ordinary delivered kind:10063 [Row]. Absence, replacement,
 * deletion, expiry, acquisition evidence, and account rerooting stay on the
 * surrounding query; this creates no second cache. */
fun decodeBlossomServerList(row: Row): BlossomServerList =
    BlossomServerList.from(
        ffiDecodeBlossomServerList(
            FfiRow(
                row.id,
                row.pubkey,
                row.createdAt,
                row.kind,
                row.tags,
                row.content,
                row.sig,
                row.sources,
            ),
        ),
    )

/** Explicit provenance-combination policy for endpoint qualification. */
enum class BlossomServerCandidatePolicy {
    SIGNED_LIST_ONLY,
    OPERATOR_ONLY,
    SIGNED_LIST_THEN_OPERATOR,
    ;

    internal fun toFfi(): FfiBlossomServerCandidatePolicy =
        when (this) {
            SIGNED_LIST_ONLY -> FfiBlossomServerCandidatePolicy.SIGNED_LIST_ONLY
            OPERATOR_ONLY -> FfiBlossomServerCandidatePolicy.OPERATOR_ONLY
            SIGNED_LIST_THEN_OPERATOR -> FfiBlossomServerCandidatePolicy.SIGNED_LIST_THEN_OPERATOR
        }
}

/** Authority that contributed one candidate. */
enum class BlossomServerCandidateSource {
    SIGNED_LIST,
    OPERATOR_CONFIG,
    ;

    companion object {
        internal fun from(ffi: FfiBlossomServerCandidateSource): BlossomServerCandidateSource =
            when (ffi) {
                FfiBlossomServerCandidateSource.SIGNED_LIST -> SIGNED_LIST
                FfiBlossomServerCandidateSource.OPERATOR_CONFIG -> OPERATOR_CONFIG
            }
    }
}

/** Syntax plus DNS/SSRF qualification evidence. Only [Admitted] is
 * selectable, and the actual HTTP operation repeats the network gate. */
sealed class BlossomServerAdmission {
    data class Admitted(
        val resolvedAddresses: List<String>,
        val operatorLocalOverride: Boolean,
    ) : BlossomServerAdmission()

    data class InvalidUrl(val error: BlossomServerUrlError) : BlossomServerAdmission()

    data class LocalHostNotAdmitted(val host: String) : BlossomServerAdmission()

    data class DnsRefused(val reason: String) : BlossomServerAdmission()

    companion object {
        internal fun from(ffi: FfiBlossomServerAdmission): BlossomServerAdmission =
            when (ffi) {
                is FfiBlossomServerAdmission.Admitted ->
                    Admitted(ffi.resolvedAddresses, ffi.operatorLocalOverride)
                is FfiBlossomServerAdmission.InvalidUrl ->
                    InvalidUrl(BlossomServerUrlError.from(ffi.error))
                is FfiBlossomServerAdmission.LocalHostNotAdmitted ->
                    LocalHostNotAdmitted(ffi.host)
                is FfiBlossomServerAdmission.DnsRefused -> DnsRefused(ffi.reason)
            }
    }
}

/** Ordered, provenance-bearing evidence for one endpoint candidate. */
data class BlossomServerCandidateEvidence(
    val serverUrl: String,
    val source: BlossomServerCandidateSource,
    val admission: BlossomServerAdmission,
) {
    companion object {
        internal fun from(
            ffi: FfiBlossomServerCandidateEvidence,
        ): BlossomServerCandidateEvidence =
            BlossomServerCandidateEvidence(
                ffi.serverUrl,
                BlossomServerCandidateSource.from(ffi.source),
                BlossomServerAdmission.from(ffi.admission),
            )
    }
}

/** Machinery failures before candidate qualification can run. Individual
 * endpoint refusals are returned as [BlossomServerAdmission] values. */
sealed class BlossomQualificationError(message: String) : Exception(message) {
    data class RuntimeUnavailable(val reason: String) :
        BlossomQualificationError("Blossom qualification runtime unavailable: $reason")

    data class ClientBuild(val reason: String) :
        BlossomQualificationError("Blossom HTTP client construction failed: $reason")

    companion object {
        internal fun from(ffi: FfiBlossomQualificationException): BlossomQualificationError =
            when (ffi) {
                is FfiBlossomQualificationException.RuntimeUnavailable ->
                    RuntimeUnavailable(ffi.reason)
                is FfiBlossomQualificationException.ClientBuild -> ClientBuild(ffi.reason)
            }
    }
}

/** Strict BUD-02 descriptor parse refusals (`FfiBlossomDescriptorError`
 * mirror). */
sealed class BlossomDescriptorError {
    data class TooLarge(val limitBytes: ULong) : BlossomDescriptorError()

    data class Json(val reason: String) : BlossomDescriptorError()

    object MissingUrl : BlossomDescriptorError()

    object MissingSha256 : BlossomDescriptorError()

    object MissingSize : BlossomDescriptorError()

    data class BadSha256(val error: BlossomSha256HexError) : BlossomDescriptorError()

    companion object {
        internal fun from(ffi: FfiBlossomDescriptorError): BlossomDescriptorError =
            when (ffi) {
                is FfiBlossomDescriptorError.TooLarge -> TooLarge(ffi.limitBytes)
                is FfiBlossomDescriptorError.Json -> Json(ffi.reason)
                is FfiBlossomDescriptorError.MissingUrl -> MissingUrl
                is FfiBlossomDescriptorError.MissingSha256 -> MissingSha256
                is FfiBlossomDescriptorError.MissingSize -> MissingSize
                is FfiBlossomDescriptorError.BadSha256 ->
                    BadSha256(BlossomSha256HexError.from(ffi.error))
            }
    }
}

/** Draft-construction + validation failures (`FfiBlossomAuthError` mirror)
 * -- every BUD-11 clause a refused authorization failed keeps its own
 * case. */
sealed class BlossomAuthError(message: String) : Exception(message) {
    data class InvalidAuthorPubkey(val got: String) :
        BlossomAuthError("invalid author public key hex: $got")

    data class InvalidBlobSha256(val error: BlossomSha256HexError) :
        BlossomAuthError("invalid blob sha256 hex: $error")

    data class InvalidEventJson(val reason: String) :
        BlossomAuthError("authorization event does not parse: $reason")

    data class ExpirationNotAfterCreatedAt(val createdAt: ULong, val expiration: ULong) :
        BlossomAuthError("authorization expiration $expiration is not after created_at $createdAt")

    data class WrongKind(val found: UShort) :
        BlossomAuthError("authorization event kind $found is not 24242")

    data class BadSignature(val reason: String) :
        BlossomAuthError("authorization event signature is invalid: $reason")

    object MissingVerb : BlossomAuthError("authorization event has no `t` verb tag")

    object MultipleVerbs :
        BlossomAuthError("authorization event carries more than one `t` verb tag")

    data class VerbMismatch(val expected: BlossomVerb, val found: String) :
        BlossomAuthError("authorization verb $found does not match expected $expected")

    data class BlobNotBound(val expectedSha256Hex: String) :
        BlossomAuthError("authorization binds no `x` tag equal to $expectedSha256Hex")

    object MissingExpiration : BlossomAuthError("authorization event has no `expiration` tag")

    data class Expired(val expiration: ULong, val now: ULong) :
        BlossomAuthError("authorization expired: expiration $expiration is not after now $now")

    data class CreatedAtInFuture(val createdAt: ULong, val now: ULong) :
        BlossomAuthError("authorization created_at $createdAt is after now $now")

    companion object {
        internal fun from(ffi: FfiBlossomAuthException): BlossomAuthError =
            when (ffi) {
                is FfiBlossomAuthException.InvalidAuthorPubkey -> InvalidAuthorPubkey(ffi.got)
                is FfiBlossomAuthException.InvalidBlobSha256 ->
                    InvalidBlobSha256(BlossomSha256HexError.from(ffi.error))
                is FfiBlossomAuthException.InvalidEventJson -> InvalidEventJson(ffi.reason)
                is FfiBlossomAuthException.ExpirationNotAfterCreatedAt ->
                    ExpirationNotAfterCreatedAt(ffi.createdAtSecs, ffi.expirationSecs)
                is FfiBlossomAuthException.WrongKind -> WrongKind(ffi.found)
                is FfiBlossomAuthException.BadSignature -> BadSignature(ffi.reason)
                is FfiBlossomAuthException.MissingVerb -> MissingVerb
                is FfiBlossomAuthException.MultipleVerbs -> MultipleVerbs
                is FfiBlossomAuthException.VerbMismatch ->
                    VerbMismatch(BlossomVerb.from(ffi.expected), ffi.found)
                is FfiBlossomAuthException.BlobNotBound -> BlobNotBound(ffi.expectedSha256Hex)
                is FfiBlossomAuthException.MissingExpiration -> MissingExpiration
                is FfiBlossomAuthException.Expired -> Expired(ffi.expirationSecs, ffi.nowSecs)
                is FfiBlossomAuthException.CreatedAtInFuture ->
                    CreatedAtInFuture(ffi.createdAtSecs, ffi.nowSecs)
            }
    }
}

/** `BlossomClient.upload`'s exhaustive failure taxonomy
 * (`FfiBlossomUploadError` mirror) -- never collapsed with the other
 * operations'. */
sealed class BlossomUploadError(message: String) : Exception(message) {
    data class InvalidServerUrl(val error: BlossomServerUrlError) :
        BlossomUploadError("invalid Blossom server URL: $error")

    data class RuntimeUnavailable(val reason: String) :
        BlossomUploadError("Blossom upload runtime unavailable: $reason")

    data class ClientBuild(val reason: String) :
        BlossomUploadError("Blossom HTTP client construction failed: $reason")

    data class AuthorizationBlobMismatch(
        val expectedSha256Hex: String,
        val authorizedVerb: BlossomVerb,
        val authorizedBlobSha256Hex: String?,
    ) : BlossomUploadError(
            "authorization ($authorizedVerb, blob $authorizedBlobSha256Hex) does not grant " +
                "uploading blob $expectedSha256Hex",
        )

    data class LocalHostNotAdmitted(val host: String) :
        BlossomUploadError("refusing Blossom upload: host $host is local and not opted-in")

    data class Network(val detail: String) :
        BlossomUploadError("Blossom upload transport failed: $detail")

    data class RedirectRefused(val status: UShort) :
        BlossomUploadError("Blossom upload redirects are not followed (HTTP $status)")

    data class AuthRejected(val status: UShort, val reason: String?) :
        BlossomUploadError("Blossom server rejected the authorization (HTTP $status: $reason)")

    data class ServerRejected(val status: UShort, val reason: String?) :
        BlossomUploadError("Blossom server rejected the upload (HTTP $status: $reason)")

    data class ServerError(val status: UShort, val reason: String?) :
        BlossomUploadError("Blossom server failed (HTTP $status: $reason)")

    data class ResponseTooLarge(val limitBytes: ULong) :
        BlossomUploadError("Blossom descriptor response exceeds $limitBytes bytes")

    data class DescriptorInvalid(val error: BlossomDescriptorError) :
        BlossomUploadError("Blossom descriptor invalid: $error")

    data class Sha256Mismatch(
        val expectedSha256Hex: String,
        val returnedSha256Hex: String,
    ) : BlossomUploadError(
            "Blossom server returned sha256 $returnedSha256Hex for a blob hashing to " +
                "$expectedSha256Hex -- refusing the descriptor",
        )

    companion object {
        internal fun from(ffi: FfiBlossomUploadException): BlossomUploadError =
            when (ffi) {
                is FfiBlossomUploadException.InvalidServerUrl ->
                    InvalidServerUrl(BlossomServerUrlError.from(ffi.error))
                is FfiBlossomUploadException.RuntimeUnavailable -> RuntimeUnavailable(ffi.reason)
                is FfiBlossomUploadException.ClientBuild -> ClientBuild(ffi.reason)
                is FfiBlossomUploadException.AuthorizationBlobMismatch ->
                    AuthorizationBlobMismatch(
                        ffi.expectedSha256Hex,
                        BlossomVerb.from(ffi.authorizedVerb),
                        ffi.authorizedBlobSha256Hex,
                    )
                is FfiBlossomUploadException.LocalHostNotAdmitted -> LocalHostNotAdmitted(ffi.host)
                is FfiBlossomUploadException.Network -> Network(ffi.detail)
                is FfiBlossomUploadException.RedirectRefused -> RedirectRefused(ffi.status)
                is FfiBlossomUploadException.AuthRejected -> AuthRejected(ffi.status, ffi.reason)
                is FfiBlossomUploadException.ServerRejected ->
                    ServerRejected(ffi.status, ffi.reason)
                is FfiBlossomUploadException.ServerException -> ServerError(ffi.status, ffi.reason)
                is FfiBlossomUploadException.ResponseTooLarge -> ResponseTooLarge(ffi.limitBytes)
                is FfiBlossomUploadException.DescriptorInvalid ->
                    DescriptorInvalid(BlossomDescriptorError.from(ffi.error))
                is FfiBlossomUploadException.Sha256Mismatch ->
                    Sha256Mismatch(ffi.expectedSha256Hex, ffi.returnedSha256Hex)
            }
    }
}

/** `BlossomClient.mirror`'s exhaustive failure taxonomy
 * (`FfiBlossomMirrorError` mirror) -- the server's 409 hash refusal and
 * 502 origin-fetch failure keep their own cases, distinct from the
 * client-side `Sha256Mismatch` integrity gate. */
sealed class BlossomMirrorError(message: String) : Exception(message) {
    data class InvalidServerUrl(val error: BlossomServerUrlError) :
        BlossomMirrorError("invalid Blossom server URL: $error")

    data class InvalidExpectedSha256(val error: BlossomSha256HexError) :
        BlossomMirrorError("invalid expected sha256 hex: $error")

    data class RuntimeUnavailable(val reason: String) :
        BlossomMirrorError("Blossom mirror runtime unavailable: $reason")

    data class ClientBuild(val reason: String) :
        BlossomMirrorError("Blossom HTTP client construction failed: $reason")

    data class AuthorizationBlobMismatch(
        val expectedSha256Hex: String,
        val authorizedVerb: BlossomVerb,
        val authorizedBlobSha256Hex: String?,
    ) : BlossomMirrorError(
            "authorization ($authorizedVerb, blob $authorizedBlobSha256Hex) does not grant " +
                "mirroring blob $expectedSha256Hex",
        )

    data class LocalHostNotAdmitted(val host: String) :
        BlossomMirrorError("refusing Blossom mirror: host $host is local and not opted-in")

    data class Network(val detail: String) :
        BlossomMirrorError("Blossom mirror transport failed: $detail")

    data class RedirectRefused(val status: UShort) :
        BlossomMirrorError("Blossom mirror redirects are not followed (HTTP $status)")

    data class AuthRejected(val status: UShort, val reason: String?) :
        BlossomMirrorError("Blossom server rejected the authorization (HTTP $status: $reason)")

    data class HashMismatchRefused(val reason: String?) :
        BlossomMirrorError(
            "Blossom server refused the mirror: mirrored blob hash does not match the " +
                "authorized x tag (HTTP 409: $reason)",
        )

    data class OriginFetchFailed(val reason: String?) :
        BlossomMirrorError(
            "Blossom server could not fetch the mirror source URL (HTTP 502: $reason)",
        )

    data class ServerRejected(val status: UShort, val reason: String?) :
        BlossomMirrorError("Blossom server rejected the mirror (HTTP $status: $reason)")

    data class ServerError(val status: UShort, val reason: String?) :
        BlossomMirrorError("Blossom server failed (HTTP $status: $reason)")

    data class ResponseTooLarge(val limitBytes: ULong) :
        BlossomMirrorError("Blossom descriptor response exceeds $limitBytes bytes")

    data class DescriptorInvalid(val error: BlossomDescriptorError) :
        BlossomMirrorError("Blossom descriptor invalid: $error")

    data class Sha256Mismatch(
        val expectedSha256Hex: String,
        val returnedSha256Hex: String,
    ) : BlossomMirrorError(
            "Blossom server returned sha256 $returnedSha256Hex for a mirror authorized as " +
                "$expectedSha256Hex -- refusing the descriptor",
        )

    companion object {
        internal fun from(ffi: FfiBlossomMirrorException): BlossomMirrorError =
            when (ffi) {
                is FfiBlossomMirrorException.InvalidServerUrl ->
                    InvalidServerUrl(BlossomServerUrlError.from(ffi.error))
                is FfiBlossomMirrorException.InvalidExpectedSha256 ->
                    InvalidExpectedSha256(BlossomSha256HexError.from(ffi.error))
                is FfiBlossomMirrorException.RuntimeUnavailable -> RuntimeUnavailable(ffi.reason)
                is FfiBlossomMirrorException.ClientBuild -> ClientBuild(ffi.reason)
                is FfiBlossomMirrorException.AuthorizationBlobMismatch ->
                    AuthorizationBlobMismatch(
                        ffi.expectedSha256Hex,
                        BlossomVerb.from(ffi.authorizedVerb),
                        ffi.authorizedBlobSha256Hex,
                    )
                is FfiBlossomMirrorException.LocalHostNotAdmitted -> LocalHostNotAdmitted(ffi.host)
                is FfiBlossomMirrorException.Network -> Network(ffi.detail)
                is FfiBlossomMirrorException.RedirectRefused -> RedirectRefused(ffi.status)
                is FfiBlossomMirrorException.AuthRejected -> AuthRejected(ffi.status, ffi.reason)
                is FfiBlossomMirrorException.HashMismatchRefused -> HashMismatchRefused(ffi.reason)
                is FfiBlossomMirrorException.OriginFetchFailed -> OriginFetchFailed(ffi.reason)
                is FfiBlossomMirrorException.ServerRejected ->
                    ServerRejected(ffi.status, ffi.reason)
                is FfiBlossomMirrorException.ServerException -> ServerError(ffi.status, ffi.reason)
                is FfiBlossomMirrorException.ResponseTooLarge -> ResponseTooLarge(ffi.limitBytes)
                is FfiBlossomMirrorException.DescriptorInvalid ->
                    DescriptorInvalid(BlossomDescriptorError.from(ffi.error))
                is FfiBlossomMirrorException.Sha256Mismatch ->
                    Sha256Mismatch(ffi.expectedSha256Hex, ffi.returnedSha256Hex)
            }
    }
}

/** `BlossomClient.delete`'s exhaustive failure taxonomy
 * (`FfiBlossomDeleteError` mirror) -- 404 keeps its own `NotFound` case
 * ("already gone" is actionable for idempotent callers). */
sealed class BlossomDeleteError(message: String) : Exception(message) {
    data class InvalidServerUrl(val error: BlossomServerUrlError) :
        BlossomDeleteError("invalid Blossom server URL: $error")

    data class InvalidBlobSha256(val error: BlossomSha256HexError) :
        BlossomDeleteError("invalid blob sha256 hex: $error")

    data class RuntimeUnavailable(val reason: String) :
        BlossomDeleteError("Blossom delete runtime unavailable: $reason")

    data class ClientBuild(val reason: String) :
        BlossomDeleteError("Blossom HTTP client construction failed: $reason")

    data class AuthorizationBlobMismatch(
        val expectedSha256Hex: String,
        val authorizedVerb: BlossomVerb,
        val authorizedBlobSha256Hex: String?,
    ) : BlossomDeleteError(
            "authorization ($authorizedVerb, blob $authorizedBlobSha256Hex) does not grant " +
                "deleting blob $expectedSha256Hex",
        )

    data class LocalHostNotAdmitted(val host: String) :
        BlossomDeleteError("refusing Blossom delete: host $host is local and not opted-in")

    data class Network(val detail: String) :
        BlossomDeleteError("Blossom delete transport failed: $detail")

    data class RedirectRefused(val status: UShort) :
        BlossomDeleteError("Blossom delete redirects are not followed (HTTP $status)")

    data class AuthRejected(val status: UShort, val reason: String?) :
        BlossomDeleteError("Blossom server rejected the authorization (HTTP $status: $reason)")

    data class NotFound(val reason: String?) :
        BlossomDeleteError("Blossom server has no such blob (HTTP 404: $reason)")

    data class ServerRejected(val status: UShort, val reason: String?) :
        BlossomDeleteError("Blossom server rejected the delete (HTTP $status: $reason)")

    data class ServerError(val status: UShort, val reason: String?) :
        BlossomDeleteError("Blossom server failed (HTTP $status: $reason)")

    companion object {
        internal fun from(ffi: FfiBlossomDeleteException): BlossomDeleteError =
            when (ffi) {
                is FfiBlossomDeleteException.InvalidServerUrl ->
                    InvalidServerUrl(BlossomServerUrlError.from(ffi.error))
                is FfiBlossomDeleteException.InvalidBlobSha256 ->
                    InvalidBlobSha256(BlossomSha256HexError.from(ffi.error))
                is FfiBlossomDeleteException.RuntimeUnavailable -> RuntimeUnavailable(ffi.reason)
                is FfiBlossomDeleteException.ClientBuild -> ClientBuild(ffi.reason)
                is FfiBlossomDeleteException.AuthorizationBlobMismatch ->
                    AuthorizationBlobMismatch(
                        ffi.expectedSha256Hex,
                        BlossomVerb.from(ffi.authorizedVerb),
                        ffi.authorizedBlobSha256Hex,
                    )
                is FfiBlossomDeleteException.LocalHostNotAdmitted -> LocalHostNotAdmitted(ffi.host)
                is FfiBlossomDeleteException.Network -> Network(ffi.detail)
                is FfiBlossomDeleteException.RedirectRefused -> RedirectRefused(ffi.status)
                is FfiBlossomDeleteException.AuthRejected -> AuthRejected(ffi.status, ffi.reason)
                is FfiBlossomDeleteException.NotFound -> NotFound(ffi.reason)
                is FfiBlossomDeleteException.ServerRejected ->
                    ServerRejected(ffi.status, ffi.reason)
                is FfiBlossomDeleteException.ServerException -> ServerError(ffi.status, ffi.reason)
            }
    }
}

/** `BlossomClient.list`'s exhaustive failure taxonomy
 * (`FfiBlossomListError` mirror) -- one malformed row fails the whole call
 * typed, never a silently shortened success. */
sealed class BlossomListError(message: String) : Exception(message) {
    data class InvalidServerUrl(val error: BlossomServerUrlError) :
        BlossomListError("invalid Blossom server URL: $error")

    data class InvalidOwnerPubkey(val got: String) :
        BlossomListError("invalid owner public key hex: $got")

    data class InvalidCursor(val error: BlossomSha256HexError) :
        BlossomListError("invalid list cursor sha256 hex: $error")

    data class RuntimeUnavailable(val reason: String) :
        BlossomListError("Blossom list runtime unavailable: $reason")

    data class ClientBuild(val reason: String) :
        BlossomListError("Blossom HTTP client construction failed: $reason")

    data class WrongVerb(val authorizedVerb: BlossomVerb) :
        BlossomListError("authorization verb $authorizedVerb does not grant listing (need `list`)")

    data class LocalHostNotAdmitted(val host: String) :
        BlossomListError("refusing Blossom list: host $host is local and not opted-in")

    data class Network(val detail: String) :
        BlossomListError("Blossom list transport failed: $detail")

    data class RedirectRefused(val status: UShort) :
        BlossomListError("Blossom list redirects are not followed (HTTP $status)")

    data class AuthRejected(val status: UShort, val reason: String?) :
        BlossomListError("Blossom server rejected the authorization (HTTP $status: $reason)")

    data class ServerRejected(val status: UShort, val reason: String?) :
        BlossomListError("Blossom server rejected the list (HTTP $status: $reason)")

    data class ServerError(val status: UShort, val reason: String?) :
        BlossomListError("Blossom server failed (HTTP $status: $reason)")

    data class ResponseTooLarge(val limitBytes: ULong) :
        BlossomListError("Blossom list response exceeds $limitBytes bytes")

    data class BodyNotAnArray(val reason: String) :
        BlossomListError("Blossom list body is not a JSON array: $reason")

    data class InvalidDescriptor(val index: ULong, val error: BlossomDescriptorError) :
        BlossomListError("Blossom list element $index is not a valid blob descriptor: $error")

    companion object {
        internal fun from(ffi: FfiBlossomListException): BlossomListError =
            when (ffi) {
                is FfiBlossomListException.InvalidServerUrl ->
                    InvalidServerUrl(BlossomServerUrlError.from(ffi.error))
                is FfiBlossomListException.InvalidOwnerPubkey -> InvalidOwnerPubkey(ffi.got)
                is FfiBlossomListException.InvalidCursor ->
                    InvalidCursor(BlossomSha256HexError.from(ffi.error))
                is FfiBlossomListException.RuntimeUnavailable -> RuntimeUnavailable(ffi.reason)
                is FfiBlossomListException.ClientBuild -> ClientBuild(ffi.reason)
                is FfiBlossomListException.WrongVerb ->
                    WrongVerb(BlossomVerb.from(ffi.authorizedVerb))
                is FfiBlossomListException.LocalHostNotAdmitted -> LocalHostNotAdmitted(ffi.host)
                is FfiBlossomListException.Network -> Network(ffi.detail)
                is FfiBlossomListException.RedirectRefused -> RedirectRefused(ffi.status)
                is FfiBlossomListException.AuthRejected -> AuthRejected(ffi.status, ffi.reason)
                is FfiBlossomListException.ServerRejected ->
                    ServerRejected(ffi.status, ffi.reason)
                is FfiBlossomListException.ServerException -> ServerError(ffi.status, ffi.reason)
                is FfiBlossomListException.ResponseTooLarge -> ResponseTooLarge(ffi.limitBytes)
                is FfiBlossomListException.BodyNotAnArray -> BodyNotAnArray(ffi.reason)
                is FfiBlossomListException.InvalidDescriptor ->
                    InvalidDescriptor(ffi.index, BlossomDescriptorError.from(ffi.error))
            }
    }
}

/** An UNSIGNED kind:24242 authorization draft (`FfiBlossomAuthDraft`
 * mirror). Sign it via the engine (`signRequest` -> `NMPEngine.signEvent`)
 * or hand `unsignedEventJson` to an external signer; nothing in this SDK
 * holds keys. */
data class BlossomAuthorizationDraft(
    /** The draft as canonical unsigned-event JSON, for external signers. */
    val unsignedEventJson: String,
    /** The blob this draft binds via its `x` tag (`null` for `list`). */
    val blobSha256Hex: String?,
    /** The verb this draft grants. */
    val verb: BlossomVerb,
    val createdAt: ULong,
    val kind: UShort,
    val tags: List<List<String>>,
    val content: String,
) {
    /** The engine sign-only request for this exact draft.
     * `NMPEngine.signEvent` freezes the author from the ACTIVE ACCOUNT, so
     * the draft's `authorPubkeyHex` must be that account's pubkey. */
    val signRequest: NMPUnsignedEvent
        get() = NMPUnsignedEvent(createdAt, kind, tags, content)

    companion object {
        internal fun from(ffi: FfiBlossomAuthDraft): BlossomAuthorizationDraft =
            BlossomAuthorizationDraft(
                ffi.unsignedEventJson,
                ffi.blobSha256Hex,
                BlossomVerb.from(ffi.verb),
                ffi.createdAtSecs,
                ffi.kind,
                ffi.tags,
                ffi.content,
            )
    }
}

private inline fun <T> blossomAuthRethrowing(body: () -> T): T =
    try {
        body()
    } catch (e: FfiBlossomAuthException) {
        throw BlossomAuthError.from(e)
    }

/** Compose an UNSIGNED BUD-11 `upload` authorization draft (kind 24242).
 * BUD-04 NOTE: a mirror is authorized with THIS builder -- the spec
 * assigns mirroring the `upload` verb. Free function, no engine needed. */
fun blossomUploadAuthorizationDraft(
    authorPubkeyHex: String,
    blobSha256Hex: String,
    createdAt: ULong,
    expiration: ULong,
    description: String,
): BlossomAuthorizationDraft =
    BlossomAuthorizationDraft.from(
        blossomAuthRethrowing {
            ffiBlossomUploadAuthorizationDraft(
                authorPubkeyHex,
                blobSha256Hex,
                createdAt,
                expiration,
                description,
            )
        },
    )

/** Compose an UNSIGNED BUD-12 `delete` authorization draft. Exactly ONE
 * blob is bound (BUD-12 forbids multi-blob deletes via extra `x` tags). */
fun blossomDeleteAuthorizationDraft(
    authorPubkeyHex: String,
    blobSha256Hex: String,
    createdAt: ULong,
    expiration: ULong,
    description: String,
): BlossomAuthorizationDraft =
    BlossomAuthorizationDraft.from(
        blossomAuthRethrowing {
            ffiBlossomDeleteAuthorizationDraft(
                authorPubkeyHex,
                blobSha256Hex,
                createdAt,
                expiration,
                description,
            )
        },
    )

/** Compose an UNSIGNED BUD-12 `list` authorization draft. No `x` tag:
 * listing is scoped to a pubkey by the request path, not to any blob. */
fun blossomListAuthorizationDraft(
    authorPubkeyHex: String,
    createdAt: ULong,
    expiration: ULong,
    description: String,
): BlossomAuthorizationDraft =
    BlossomAuthorizationDraft.from(
        blossomAuthRethrowing {
            ffiBlossomListAuthorizationDraft(
                authorPubkeyHex,
                createdAt,
                expiration,
                description,
            )
        },
    )

/** A signed kind:24242 event PROVEN (at construction) to satisfy every
 * BUD-11 check (`FfiBlossomAuthorization` mirror) -- the only value
 * `BlossomClient`'s operations accept, so an unvalidated event can never
 * become an `Authorization` header. */
class BlossomAuthorization private constructor(internal val ffi: FfiBlossomAuthorization) {
    /** The verb this authorization was validated FOR. */
    val verb: BlossomVerb
        get() = BlossomVerb.from(ffi.verb())

    /** The blob hash this authorization was proven to bind (`null` for
     * verbs validated without a blob binding). */
    val blobSha256Hex: String?
        get() = ffi.blobSha256Hex()

    companion object {
        /** Fail-closed BUD-11 validation of a signed event supplied as
         * canonical event JSON (the external-signer path). [verb] is what
         * the caller is ABOUT to use the authorization for;
         * [blobSha256Hex] binds the exact blob for verbs that grant one
         * (`UPLOAD`/`DELETE`; mirror validates under `UPLOAD`); [now] is
         * the caller's clock (unix seconds). */
        fun validate(
            signedEventJson: String,
            verb: BlossomVerb,
            blobSha256Hex: String?,
            now: ULong,
        ): BlossomAuthorization =
            BlossomAuthorization(
                blossomAuthRethrowing {
                    FfiBlossomAuthorization.validate(
                        signedEventJson,
                        verb.toFfi(),
                        blobSha256Hex,
                        now,
                    )
                },
            )

        /** Fail-closed BUD-11 validation of the exact value
         * `NMPEngine.signEvent` returns (the engine sign-only path) -- the
         * same checks as the JSON door. */
        fun validate(
            signedEvent: NMPSignedEvent,
            verb: BlossomVerb,
            blobSha256Hex: String?,
            now: ULong,
        ): BlossomAuthorization =
            BlossomAuthorization(
                blossomAuthRethrowing {
                    FfiBlossomAuthorization.validateSignedEvent(
                        FfiSignedEvent(
                            id = signedEvent.id,
                            pubkey = signedEvent.pubkey,
                            createdAt = signedEvent.createdAt,
                            kind = signedEvent.kind,
                            tags = signedEvent.tags,
                            content = signedEvent.content,
                            sig = signedEvent.signature,
                        ),
                        verb.toFfi(),
                        blobSha256Hex,
                        now,
                    )
                },
            )
    }
}

/** `BlossomClient` construction knobs (`FfiBlossomClientConfig` mirror).
 * `null` means the Rust crate's default. */
data class BlossomClientConfig(
    /** Operator opt-in local-host allowlist (normalized bare-host form,
     * lowercase). Empty means NO loopback/private/link-local/onion host
     * may be dialed. */
    val allowedLocalHosts: List<String> = emptyList(),
    /** Cap on a single-descriptor response body (upload/mirror). */
    val maxResponseBytes: ULong? = null,
    /** Cap on a `GET /list` response body. */
    val maxListResponseBytes: ULong? = null,
    /** Overall request deadline (connect, headers, and body), seconds. */
    val requestDeadlineSeconds: ULong? = null,
) {
    internal fun toFfi(): FfiBlossomClientConfig =
        FfiBlossomClientConfig(
            allowedLocalHosts = allowedLocalHosts,
            maxResponseBytes = maxResponseBytes,
            maxListResponseBytes = maxListResponseBytes,
            requestDeadlineSecs = requestDeadlineSeconds,
        )
}

/** The BUD-02/04/12 blob client (`FfiBlossomClient` mirror). Every method
 * is a suspend function running the underlying BLOCKING FFI call on
 * `Dispatchers.IO`; each failure arrives as that operation's own typed
 * sealed error class. */
class BlossomClient(config: BlossomClientConfig = BlossomClientConfig()) {
    internal val ffi: FfiBlossomClient = FfiBlossomClient(config.toFfi())

    /** Apply one explicit configuration/list policy and return admission
     * evidence for every candidate in selection order. No HTTP request is
     * sent. A signed list never grants local-network access; only the
     * client's operator allowlist can produce `operatorLocalOverride`. */
    suspend fun qualifyServerCandidates(
        policy: BlossomServerCandidatePolicy,
        operatorServerUrls: List<String> = emptyList(),
        signedList: BlossomServerList? = null,
    ): List<BlossomServerCandidateEvidence> =
        withContext(Dispatchers.IO) {
            try {
                ffi.qualifyServerCandidates(
                    policy.toFfi(),
                    operatorServerUrls,
                    signedList?.toFfi(),
                ).map { BlossomServerCandidateEvidence.from(it) }
            } catch (e: FfiBlossomQualificationException) {
                throw BlossomQualificationError.from(e)
            }
        }

    /** `PUT /upload` of [blob]'s exact bytes -- self-verifying end to end:
     * the returned descriptor's sha256 was PROVEN equal to the hash of the
     * uploaded bytes. [authorization] must be an `upload` grant bound to
     * exactly those bytes. */
    suspend fun upload(
        serverUrl: String,
        blob: ByteArray,
        contentType: String? = null,
        authorization: BlossomAuthorization,
    ): BlobDescriptor =
        withContext(Dispatchers.IO) {
            try {
                BlobDescriptor.from(
                    ffi.upload(
                        serverUrl,
                        blob,
                        contentType,
                        authorization.ffi,
                    ),
                )
            } catch (e: FfiBlossomUploadException) {
                throw BlossomUploadError.from(e)
            }
        }

    /** `PUT /mirror` (BUD-04): ask [serverUrl] to download the blob at
     * [sourceUrl] itself, integrity-gated against [expectedSha256Hex].
     * [authorization] is an `upload` grant bound to that hash. */
    suspend fun mirror(
        serverUrl: String,
        sourceUrl: String,
        expectedSha256Hex: String,
        authorization: BlossomAuthorization,
    ): BlobDescriptor =
        withContext(Dispatchers.IO) {
            try {
                BlobDescriptor.from(
                    ffi.mirror(serverUrl, sourceUrl, expectedSha256Hex, authorization.ffi),
                )
            } catch (e: FfiBlossomMirrorException) {
                throw BlossomMirrorError.from(e)
            }
        }

    /** `DELETE /<sha256>` (BUD-12). [authorization] is a `delete` grant
     * bound to EXACTLY [blobSha256Hex]; 404 surfaces as
     * [BlossomDeleteError.NotFound] ("already gone"). */
    suspend fun delete(
        serverUrl: String,
        blobSha256Hex: String,
        authorization: BlossomAuthorization,
    ) {
        withContext(Dispatchers.IO) {
            try {
                ffi.delete(serverUrl, blobSha256Hex, authorization.ffi)
            } catch (e: FfiBlossomDeleteException) {
                throw BlossomDeleteError.from(e)
            }
        }
    }

    /** `GET /list/<pubkey>` (BUD-12): the blobs [serverUrl] stores for
     * [ownerPubkeyHex], newest first. [authorization] is optional -- a
     * server that requires a `list` grant answers 401, surfaced as
     * [BlossomListError.AuthRejected]. [cursorSha256Hex]/[limit] are the
     * BUD-12 pagination parameters, sent only when set. */
    suspend fun list(
        serverUrl: String,
        ownerPubkeyHex: String,
        cursorSha256Hex: String? = null,
        limit: UInt? = null,
        authorization: BlossomAuthorization? = null,
    ): List<BlobDescriptor> =
        withContext(Dispatchers.IO) {
            try {
                ffi.list(serverUrl, ownerPubkeyHex, cursorSha256Hex, limit, authorization?.ffi)
                    .map { BlobDescriptor.from(it) }
            } catch (e: FfiBlossomListException) {
                throw BlossomListError.from(e)
            }
        }
}
