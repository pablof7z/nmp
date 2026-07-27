// The read-only NIP-29 group-discovery projection (#108/#838) -- a pure
// function requiring no `NMPEngine` instance. NIP-29 deliberately exposes
// neither a fixed content-kind catalog nor a kind:9 composer; those belong to
// the app's selected schema modules and notification policy.

import NMPFFI

/// Group discovery (kind:39000) pinned to `host` (#108). Throws
/// `NMPError.invalidRelayUrl` if `host` doesn't parse.
public func groupDiscoveryDemand(host: String) throws -> NMPDemand {
    try NMPDemand(nmpRethrowing { try NMPFFI.groupDiscoveryDemand(host: host) })
}
