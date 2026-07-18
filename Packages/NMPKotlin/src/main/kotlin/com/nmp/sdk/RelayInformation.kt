package com.nmp.sdk

import uniffi.nmp_ffi.FfiRelayInformation
import uniffi.nmp_ffi.FfiRelayInformationCachePolicy
import uniffi.nmp_ffi.FfiRelayInformationDocument
import uniffi.nmp_ffi.FfiRelayInformationErrorKind
import uniffi.nmp_ffi.FfiRelayInformationFreshness
import uniffi.nmp_ffi.FfiRelayInformationLimitations

enum class RelayInformationCachePolicy {
    UseCache,
    Refresh;

    internal fun toFfi(): FfiRelayInformationCachePolicy =
        when (this) {
            UseCache -> FfiRelayInformationCachePolicy.USE_CACHE
            Refresh -> FfiRelayInformationCachePolicy.REFRESH
        }
}

/** Advisory limits claimed by the relay. Omitted fields remain null; they
 * are never inferred as zero/false or treated as runtime proof. */
data class RelayInformationLimitations(
    val maxMessageLength: ULong?,
    val maxSubscriptions: ULong?,
    val maxFilters: ULong?,
    val maxLimit: ULong?,
    val maxSubidLength: ULong?,
    val maxEventTags: ULong?,
    val maxContentLength: ULong?,
    val minPowDifficulty: ULong?,
    val authRequired: Boolean?,
    val paymentRequired: Boolean?,
    val createdAtLowerLimit: ULong?,
    val createdAtUpperLimit: ULong?,
) {
    companion object {
        internal fun from(ffi: FfiRelayInformationLimitations) =
            RelayInformationLimitations(
                maxMessageLength = ffi.maxMessageLength,
                maxSubscriptions = ffi.maxSubscriptions,
                maxFilters = ffi.maxFilters,
                maxLimit = ffi.maxLimit,
                maxSubidLength = ffi.maxSubidLength,
                maxEventTags = ffi.maxEventTags,
                maxContentLength = ffi.maxContentLength,
                minPowDifficulty = ffi.minPowDifficulty,
                authRequired = ffi.authRequired,
                paymentRequired = ffi.paymentRequired,
                createdAtLowerLimit = ffi.createdAtLowerLimit,
                createdAtUpperLimit = ffi.createdAtUpperLimit,
            )
    }
}

enum class RelayInformationFreshness {
    Fresh,
    Stale;

    companion object {
        internal fun from(ffi: FfiRelayInformationFreshness): RelayInformationFreshness =
            when (ffi) {
                FfiRelayInformationFreshness.FRESH -> Fresh
                FfiRelayInformationFreshness.STALE -> Stale
            }
    }
}

/** Typed failure of one bounded NIP-11 acquisition (mirrors `nmp-ffi`'s own
 * `FfiRelayInformationErrorKind`; see that type's doc for the Rust side of
 * each case). Carried by [RelayInformation.lastError] as stale-on-error
 * evidence, and by `NMPError.RelayInformationUnavailable` when acquisition
 * fails before any last-good document exists. */
sealed interface RelayInformationErrorKind {
    data class WaiterSaturated(val capacity: ULong) : RelayInformationErrorKind
    data class ThreadUnavailable(val reason: String) : RelayInformationErrorKind
    data object ServiceClosed : RelayInformationErrorKind
    data object CredentialedRelayUrl : RelayInformationErrorKind
    data class Http(val reason: String) : RelayInformationErrorKind
    data class ResponseTooLarge(val limitBytes: ULong) : RelayInformationErrorKind
    data class InvalidDocument(val reason: String) : RelayInformationErrorKind

    companion object {
        internal fun from(ffi: FfiRelayInformationErrorKind): RelayInformationErrorKind =
            when (ffi) {
                is FfiRelayInformationErrorKind.WaiterSaturated -> WaiterSaturated(ffi.capacity)
                is FfiRelayInformationErrorKind.ThreadUnavailable -> ThreadUnavailable(ffi.reason)
                FfiRelayInformationErrorKind.ServiceClosed -> ServiceClosed
                FfiRelayInformationErrorKind.CredentialedRelayUrl -> CredentialedRelayUrl
                is FfiRelayInformationErrorKind.Http -> Http(ffi.reason)
                is FfiRelayInformationErrorKind.ResponseTooLarge -> ResponseTooLarge(ffi.limitBytes)
                is FfiRelayInformationErrorKind.InvalidDocument -> InvalidDocument(ffi.reason)
            }
    }
}

/** Human-readable text mirroring `nmp::RelayInformationError`'s own
 * `Display` impl (crates/nmp/src/relay_information.rs), for callers that
 * only want a message rather than a branch on the typed kind. */
fun RelayInformationErrorKind.describe(): String =
    when (this) {
        is RelayInformationErrorKind.WaiterSaturated ->
            "NIP-11 acquisition refused: per-relay waiter capacity $capacity is full"
        is RelayInformationErrorKind.ThreadUnavailable ->
            "NIP-11 acquisition thread unavailable: $reason"
        RelayInformationErrorKind.ServiceClosed -> "NIP-11 acquisition service is closed"
        RelayInformationErrorKind.CredentialedRelayUrl ->
            "NIP-11 acquisition refuses relay URL userinfo"
        is RelayInformationErrorKind.Http -> "NIP-11 HTTP request failed: $reason"
        is RelayInformationErrorKind.ResponseTooLarge -> "NIP-11 response exceeds $limitBytes bytes"
        is RelayInformationErrorKind.InvalidDocument -> "invalid NIP-11 document: $reason"
    }

data class RelayInformationDocument(
    val name: String?,
    val description: String?,
    val banner: String?,
    val icon: String?,
    val pubkey: String?,
    val selfPubkey: String?,
    val contact: String?,
    /** `null` means no list advertised; empty means explicitly none. */
    val supportedNips: List<UShort>?,
    val software: String?,
    val version: String?,
    val termsOfService: String?,
    val limitation: RelayInformationLimitations,
    val structured: Map<String, String>,
) {
    companion object {
        internal fun from(ffi: FfiRelayInformationDocument) =
            RelayInformationDocument(
                name = ffi.name,
                description = ffi.description,
                banner = ffi.banner,
                icon = ffi.icon,
                pubkey = ffi.pubkey,
                selfPubkey = ffi.selfPubkey,
                contact = ffi.contact,
                supportedNips = ffi.supportedNips,
                software = ffi.software,
                version = ffi.version,
                termsOfService = ffi.termsOfService,
                limitation = RelayInformationLimitations.from(ffi.limitation),
                structured = ffi.structured,
            )
    }
}

data class RelayInformation(
    val relay: String,
    val document: RelayInformationDocument,
    val rawJson: String,
    val documentRevision: String,
    val fetchedAt: ULong,
    val freshUntil: ULong,
    val freshness: RelayInformationFreshness,
    val etag: String?,
    val lastModified: String?,
    val cacheControl: String?,
    val expires: String?,
    val lastError: RelayInformationErrorKind?,
) {
    companion object {
        internal fun from(ffi: FfiRelayInformation) =
            RelayInformation(
                relay = ffi.relay,
                document = RelayInformationDocument.from(ffi.document),
                rawJson = ffi.rawJson,
                documentRevision = ffi.documentRevision,
                fetchedAt = ffi.fetchedAt,
                freshUntil = ffi.freshUntil,
                freshness = RelayInformationFreshness.from(ffi.freshness),
                etag = ffi.etag,
                lastModified = ffi.lastModified,
                cacheControl = ffi.cacheControl,
                expires = ffi.expires,
                lastError = ffi.lastError?.let(RelayInformationErrorKind::from),
            )
    }
}
