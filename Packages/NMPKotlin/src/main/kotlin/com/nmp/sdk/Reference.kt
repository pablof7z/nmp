// Engine-free reference planning projected from nmp_grammar::reference.
// These are ordinary values only: this file opens no observation and owns no
// renderer, component lifecycle, evidence merge, or cache.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiReferenceTarget
import uniffi.nmp_ffi.referenceDemandPlan as ffiReferenceDemandPlan

/** One canonical observation plus optional acquisition helpers for a parsed
 * reference target. The purpose-owning application/component decides whether
 * to open any of them and owns each resulting Flow collection independently.
 * Only [canonical] supplies rendered winner state; helpers feed NMPs one
 * canonical store and retain their own scoped evidence. */
data class NostrReferenceDemandPlan(
    val targetKey: String,
    val canonical: NMPDemand,
    val helpers: List<NMPDemand>,
    /** Malformed, unsafe, or over-bound raw relay hints not promoted into a
     * pinned helper. Exact duplicates do not increment this value. */
    val discardedRelayHints: UInt,
)

/** Validate and lower one normalized authored target without observing it. */
fun referenceDemandPlan(target: NostrReferenceTarget): NostrReferenceDemandPlan {
    val ffi = nmpRethrowing { ffiReferenceDemandPlan(target.toFfi()) }
    return NostrReferenceDemandPlan(
        targetKey = ffi.targetKey,
        canonical = NMPDemand.from(ffi.canonical),
        helpers = ffi.helpers.map(NMPDemand::from),
        discardedRelayHints = ffi.discardedRelayHints,
    )
}

private fun NostrReferenceTarget.toFfi(): FfiReferenceTarget =
    when (this) {
        is NostrReferenceTarget.Profile -> FfiReferenceTarget.Profile(pubkey, relayHints)
        is NostrReferenceTarget.Event -> FfiReferenceTarget.Event(id, authorHint, kindHint, relayHints)
        is NostrReferenceTarget.Address ->
            FfiReferenceTarget.Address(kind, author, identifier, relayHints)
    }
