package com.nmp.sdk

import uniffi.nmp_ffi.FfiRelayInformation
import uniffi.nmp_ffi.FfiRelayInformationCachePolicy
import uniffi.nmp_ffi.FfiRelayInformationDocument
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
    val lastError: String?,
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
                lastError = ffi.lastError,
            )
    }
}
