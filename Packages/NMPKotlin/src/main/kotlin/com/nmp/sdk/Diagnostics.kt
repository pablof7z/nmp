// The diagnostic surface's delivered value types, in ergonomic Kotlin shape
// -- "the acceptance test rendered on screen, permanently." Mirrors
// Diagnostics.swift's pattern exactly: no `Ffi`-prefixed type ever leaks
// past this file.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiAuthDiagnostics
import uniffi.nmp_ffi.FfiDiagnosticsSnapshot
import uniffi.nmp_ffi.FfiCoverageInterval
import uniffi.nmp_ffi.FfiFilterCoverage
import uniffi.nmp_ffi.FfiKindCount
import uniffi.nmp_ffi.FfiLaneCount
import uniffi.nmp_ffi.FfiRelayDiagnostics

/** One (kind, count) pair -- events actually RECEIVED from a relay, counted
 * by kind. */
data class KindCount(val kind: UShort, val count: ULong) {
    companion object {
        fun from(ffi: FfiKindCount): KindCount = KindCount(ffi.kind, ffi.count)
    }
}

/** One (lane, count) pair -- how many of a relay's wire subs trace to each
 * routing lane (NIP-65 write, hint, indexer discovery, ...). */
data class LaneCount(val lane: String, val count: UInt) {
    companion object {
        fun from(ffi: FfiLaneCount): LaneCount = LaneCount(ffi.lane, ffi.count)
    }
}

/** A proven, retained `[from, through]` interval -- the diagnostics-only
 * watermark. It is deliberately distinct from query-scoped
 * `AcquisitionEvidence`. */
data class CoverageInterval(val from: ULong, val through: ULong) {
    companion object {
        fun from(ffi: FfiCoverageInterval): CoverageInterval = CoverageInterval(ffi.from, ffi.through)
    }
}

/** One filter's proven coverage state at one relay. `filter` is the EXACT
 * wire JSON this coverage state is for -- the same rendering as the
 * parallel entry in `RelayDiagnostics.filters`. `null` means this exact
 * `(relay, filter)` interval remains unproven. */
data class FilterCoverage(val filter: String, val coverage: CoverageInterval?) {
    companion object {
        fun from(ffi: FfiFilterCoverage): FilterCoverage =
            FilterCoverage(ffi.filter, ffi.coverage?.let { CoverageInterval.from(it) })
    }
}

/** One relay's full diagnostics: wire-sub count, lane breakdown, reverse
 * coverage (authors served), the exact filters currently sent, events
 * actually received per kind, and per-filter coverage state. Every field is
 * a REAL number read off the running engine -- never fabricated/estimated. */
data class RelayDiagnostics(
    val relay: String,
    /** The frozen access identity of the physical session these diagnostics
     * describe (#8): the same relay under [NMPAccessContext.Public] versus a
     * [NMPAccessContext.Nip42] identity is a distinct session with its own
     * row. */
    val access: NMPAccessContext,
    val wireSubCount: UInt,
    val authorsServed: UInt,
    val byLane: List<LaneCount>,
    /** The EXACT wire JSON of every filter currently sent to this relay. */
    val filters: List<String>,
    val eventsByKind: List<KindCount>,
    val coverage: List<FilterCoverage>,
    val nip11SupportedNips: List<UShort>?,
    val nip11DocumentRevision: String?,
    val nip11Freshness: String?,
    val nip11LastError: String?,
    val nip77Advertisement: String,
    val nip77Behavior: String,
) {
    companion object {
        fun from(ffi: FfiRelayDiagnostics): RelayDiagnostics =
            RelayDiagnostics(
                relay = ffi.relay,
                access = NMPAccessContext.from(ffi.access),
                wireSubCount = ffi.wireSubCount,
                authorsServed = ffi.authorsServed,
                byLane = ffi.byLane.map { LaneCount.from(it) },
                filters = ffi.filters,
                eventsByKind = ffi.eventsByKind.map { KindCount.from(it) },
                coverage = ffi.coverage.map { FilterCoverage.from(it) },
                nip11SupportedNips = ffi.nip11SupportedNips,
                nip11DocumentRevision = ffi.nip11DocumentRevision,
                nip11Freshness = ffi.nip11Freshness,
                nip11LastError = ffi.nip11LastError,
                nip77Advertisement = ffi.nip77Advertisement,
                nip77Behavior = ffi.nip77Behavior,
            )
    }
}

/** One bounded exact-session AUTH diagnostics record. */
data class AuthDiagnostics(
    val relay: String,
    val access: NMPAccessContext,
    val transportGeneration: ULong,
    val epochSequence: ULong?,
    val challengeDescriptor: String?,
    val phase: AuthPhase,
    val policyBound: Boolean,
    val signerBound: Boolean,
    val authEventId: String?,
    val sendHandoffAccepted: Boolean,
    val relayOkAccepted: Boolean,
) {
    companion object {
        fun from(ffi: FfiAuthDiagnostics): AuthDiagnostics =
            AuthDiagnostics(
                relay = ffi.relay,
                access = NMPAccessContext.from(ffi.access),
                transportGeneration = ffi.transportGeneration,
                epochSequence = ffi.epochSequence,
                challengeDescriptor = ffi.challengeDescriptor,
                phase = AuthPhase.from(ffi.phase),
                policyBound = ffi.policyBound,
                signerBound = ffi.signerBound,
                authEventId = ffi.authEventId,
                sendHandoffAccepted = ffi.sendHandoffAccepted,
                relayOkAccepted = ffi.relayOkAccepted,
            )
    }
}

/** The engine-global diagnostics snapshot -- one snapshot covers every
 * currently-planned relay. Delivered by `observeDiagnostics()`, pushed
 * reactively, never polled. */
data class DiagnosticsSnapshot(
    val relays: List<RelayDiagnostics> = emptyList(),
    val authSessions: List<AuthDiagnostics> = emptyList(),
    val uncoveredAuthorCount: UInt = 0u,
    val droppedMergeRules: List<String> = emptyList(),
    val transportDegraded: String? = null,
) {
    companion object {
        fun from(ffi: FfiDiagnosticsSnapshot): DiagnosticsSnapshot =
            DiagnosticsSnapshot(
                relays = ffi.relays.map { RelayDiagnostics.from(it) },
                authSessions = ffi.authSessions.map { AuthDiagnostics.from(it) },
                uncoveredAuthorCount = ffi.uncoveredAuthorCount,
                droppedMergeRules = ffi.droppedMergeRules,
                transportDegraded = ffi.transportDegraded,
            )
    }
}
