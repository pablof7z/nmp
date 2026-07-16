//! Pure relay-host classification shared by value planning and transport.
//!
//! This module performs no admission decision, DNS resolution, or I/O. It
//! classifies only the parsed host (or a resolved IP) as public versus local.
//! Provenance-aware allowlists, resolved-address pinning, and dial policy stay
//! with the engine/transport layers which own those effects.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use nostr::types::url::Host;
use nostr::RelayUrl;

/// Host-only relay classification. The value describes what a host is; it
/// does not decide whether an operator-authorized local host may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayHostClass {
    Public,
    Local,
}

/// Classify a relay URL by its host alone. Path, query, and fragment never
/// influence the verdict. A missing host fails closed as [`RelayHostClass::Local`].
#[must_use]
pub fn classify_relay_host(url: &RelayUrl) -> RelayHostClass {
    match url.host() {
        Some(Host::Ipv4(ip)) => classify_ipv4(ip),
        Some(Host::Ipv6(ip)) => classify_ipv6(ip),
        Some(Host::Domain(name)) => classify_domain(name),
        None => RelayHostClass::Local,
    }
}

/// Canonical host-only key used by provenance-aware local-host allowlists.
#[must_use]
pub fn relay_host_key(url: &RelayUrl) -> Option<String> {
    match url.host()? {
        Host::Domain(name) => Some(name.trim_end_matches('.').to_ascii_lowercase()),
        Host::Ipv4(ip) => Some(ip.to_string()),
        Host::Ipv6(ip) => Some(ip.to_string()),
    }
}

/// Classify a resolved address by the same local-range rules as a literal URL
/// host. Callers still own DNS-pinning and provenance-aware admission.
#[must_use]
pub fn classify_ip(ip: IpAddr) -> RelayHostClass {
    match ip {
        IpAddr::V4(ip) => classify_ipv4(ip),
        IpAddr::V6(ip) => classify_ipv6(ip),
    }
}

/// Normalize a bare host to the same key [`relay_host_key`] derives.
#[must_use]
pub fn normalize_bare_host(host: &str) -> String {
    let trimmed = host.trim_end_matches('.');
    match trimmed.parse::<IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => trimmed.to_ascii_lowercase(),
    }
}

fn classify_ipv4(ip: Ipv4Addr) -> RelayHostClass {
    if ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
    {
        RelayHostClass::Local
    } else {
        RelayHostClass::Public
    }
}

fn classify_ipv6(ip: Ipv6Addr) -> RelayHostClass {
    let segments = ip.segments();
    if let Some(ipv4) = ip.to_ipv4_mapped() {
        return classify_ipv4(ipv4);
    }
    if segments[..6].iter().all(|&segment| segment == 0)
        && !ip.is_unspecified()
        && !ip.is_loopback()
    {
        let ipv4 = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            (segments[6] & 0xff) as u8,
            (segments[7] >> 8) as u8,
            (segments[7] & 0xff) as u8,
        );
        return classify_ipv4(ipv4);
    }
    let unique_local = (segments[0] & 0xfe00) == 0xfc00;
    let link_local = (segments[0] & 0xffc0) == 0xfe80;
    if ip.is_loopback() || ip.is_unspecified() || unique_local || link_local {
        RelayHostClass::Local
    } else {
        RelayHostClass::Public
    }
}

fn classify_domain(name: &str) -> RelayHostClass {
    let host = name.trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".onion") {
        RelayHostClass::Local
    } else {
        RelayHostClass::Public
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(url: &str) -> RelayHostClass {
        classify_relay_host(&RelayUrl::parse(url).expect("valid test relay URL"))
    }

    #[test]
    fn path_never_changes_public_host_classification() {
        assert_eq!(
            class("wss://nostr.wine/npub1xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"),
            RelayHostClass::Public
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
            assert_eq!(class(url), RelayHostClass::Local, "{url}");
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
            assert_eq!(class(url), RelayHostClass::Public, "{url}");
        }
    }

    #[test]
    fn resolved_ip_uses_the_same_ranges_as_literal_hosts() {
        for host in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "::1",
            "fc00::1",
            "::ffff:10.0.0.1",
        ] {
            assert_eq!(
                classify_ip(host.parse().unwrap()),
                RelayHostClass::Local,
                "{host}"
            );
        }
        for host in ["8.8.8.8", "172.32.0.1", "2606:4700:4700::1111"] {
            assert_eq!(
                classify_ip(host.parse().unwrap()),
                RelayHostClass::Public,
                "{host}"
            );
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
        assert_eq!(
            normalize_bare_host("Relay.Example.COM."),
            "relay.example.com"
        );
    }
}
