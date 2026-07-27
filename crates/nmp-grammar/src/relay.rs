//! Relay-URL host-key adapter over the one pure destination-admission owner.
//!
//! Relay URL parsing and host extraction belong here. Bare-host normalization
//! belongs to `nmp-network-policy`, so every dial site compares the same
//! spelling of the same host. This module performs no admission decision, DNS
//! resolution, or I/O; classification, provenance-aware allowlists,
//! resolved-address admission, and dial policy stay with the engine/transport
//! layers which own those effects.

use nmp_network_policy::normalize_bare_host;
use nostr::types::url::Host;
use nostr::RelayUrl;

/// Canonical host-only key extracted from a relay URL, in
/// [`nmp_network_policy::normalize_bare_host`]'s form.
#[must_use]
pub fn relay_host_key(url: &RelayUrl) -> Option<String> {
    match url.host()? {
        Host::Domain(name) => Some(normalize_bare_host(name)),
        Host::Ipv4(ip) => Some(normalize_bare_host(&ip.to_string())),
        Host::Ipv6(ip) => Some(normalize_bare_host(&ip.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_keys_are_canonical_and_port_path_independent() {
        let first = RelayUrl::parse("ws://127.0.0.1:7777").unwrap();
        let second = RelayUrl::parse("ws://127.0.0.1:9999/path").unwrap();
        assert_eq!(relay_host_key(&first), relay_host_key(&second));
        assert_eq!(
            relay_host_key(&RelayUrl::parse("wss://Relay.Example.COM").unwrap()),
            Some("relay.example.com".to_string())
        );
    }
}
