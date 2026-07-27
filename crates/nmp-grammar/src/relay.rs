//! Relay-URL adapter over the one pure destination-admission owner.
//!
//! Relay URL parsing and host extraction belong here. The security-relevant
//! rules — bare-host normalization and public/local classification — belong
//! to `nmp-network-policy`, so every dial site compares the same spelling of
//! the same host. This module performs no admission decision, DNS
//! resolution, or I/O; provenance-aware allowlists, resolved-address
//! admission, and dial policy stay with the engine/transport layers which
//! own those effects.

pub use nmp_network_policy::HostClass;
use nmp_network_policy::{classify_bare_host, normalize_bare_host};
use nostr::types::url::Host;
use nostr::RelayUrl;

/// Classify a relay URL by its host alone. Path, query, and fragment never
/// influence the verdict. A missing host fails closed as [`HostClass::Local`].
#[must_use]
pub fn classify_relay_host(url: &RelayUrl) -> HostClass {
    match url.host() {
        Some(Host::Ipv4(ip)) => classify_bare_host(&ip.to_string()),
        Some(Host::Ipv6(ip)) => classify_bare_host(&ip.to_string()),
        Some(Host::Domain(name)) => classify_bare_host(name),
        None => HostClass::Local,
    }
}

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

    fn class(url: &str) -> HostClass {
        classify_relay_host(&RelayUrl::parse(url).expect("valid test relay URL"))
    }

    #[test]
    fn path_never_changes_public_host_classification() {
        assert_eq!(
            class("wss://nostr.wine/npub1xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"),
            HostClass::Public
        );
    }

    #[test]
    fn literal_local_private_onion_and_mapped_hosts_fail_closed() {
        for url in [
            "ws://127.0.0.1:7777",
            "ws://127.5.5.5",
            "ws://10.0.0.1",
            "ws://172.16.0.1",
            "ws://172.31.255.1",
            "ws://192.168.1.1",
            "ws://169.254.169.254",
            "ws://0.0.0.0",
            "ws://255.255.255.255",
            "ws://127.0.0.1.:80",
            "wss://2130706433",
            "wss://0x7f000001",
            "ws://[::1]",
            "ws://[::]",
            "ws://[fc00::1]",
            "ws://[fd12:3456::1]",
            "ws://[fe80::1]",
            "ws://[::ffff:127.0.0.1]",
            "ws://[::127.0.0.1]",
            "ws://[::7f00:1]",
            "ws://[::0a00:0005]",
            "wss://hiddenservice.onion",
            "ws://localhost:7777",
            "ws://foo.localhost",
        ] {
            assert_eq!(class(url), HostClass::Local, "{url}");
        }
    }

    #[test]
    fn public_ranges_and_local_looking_public_domains_stay_public() {
        for url in [
            "wss://relay.damus.io",
            "wss://localhost.example.com",
            "ws://172.32.0.1",
            "ws://8.8.8.8",
            "ws://1.1.1.1",
            "ws://[2606:4700:4700::1111]",
        ] {
            assert_eq!(class(url), HostClass::Public, "{url}");
        }
    }

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
