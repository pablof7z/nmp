// The read-only NIP-29 group-discovery projection (#108/#838). NIP-29
// deliberately exposes neither a fixed content-kind catalog nor a kind:9
// composer; those belong to independently optional schema modules and client
// notification policy.

package com.nmp.sdk

import uniffi.nmp_ffi.groupDiscoveryDemand as ffiGroupDiscoveryDemand

/** Group discovery (kind:39000) pinned to [host] (#108). Throws
 * `NMPError.InvalidRelayUrl` if [host] doesn't parse. */
fun groupDiscoveryDemand(host: String): NMPDemand =
    NMPDemand.from(nmpRethrowing { ffiGroupDiscoveryDemand(host) })
