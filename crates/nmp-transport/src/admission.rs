//! Pure, HOST-ONLY relay-URL admission classification (issue #121).
//!
//! Discovered relay URLs (network-sourced NIP-65 kind:10002 / kind:10050
//! lists) reach this transport pool with no notion of "who vouched for this
//! host". A validly-signed kind:10002 can still name a *hostile* target — a
//! loopback/LAN address the app process can reach but the wider network
//! cannot (an SSRF pivot), or a `.onion` host that silently drags every dial
//! onto Tor. This module answers exactly one question, with no I/O and no
//! DNS: **is this URL's HOST one that only a trust boundary the network
//! cannot vouch for could route to?**
//!
//! The classification is deliberately kept here (a pure, reusable fn) rather
//! than folded into an admission *decision*: transport cannot know a URL's
//! provenance (discovered vs. explicitly user-configured), so it cannot know
//! whether a `Local` host should be *allowed* — only whether it *is* local.
//! The provenance-aware decision lives in the engine (`nmp-engine`'s relay
//! admission), which calls this classifier and then applies the operator's
//! opt-in allowlist. See issue #121.
//!
//! ## Host, never path
//!
//! Every check keys on the parsed HOST component alone. A per-user relay that
//! lives at a URL PATH — `wss://nostr.wine/<npub>` — is a perfectly ordinary
//! public relay and MUST be admitted; nothing about the path is ever
//! consulted. `classify_relay_host` reads `RelayUrl::host()` and nothing
//! else, so a path can never influence the verdict. The
//! `path_is_never_consulted_public_host_at_a_path_is_public` falsifier pins
//! that exact URL as `Public`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use nostr::types::url::Host;
use nostr::RelayUrl;

/// The HOST-only classification of a relay URL. Intentionally two-valued: it
/// reports *what the host is*, never *whether to admit it* — that decision
/// needs provenance this crate does not have (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayHostClass {
    /// A globally-routable public host — safe to dial from any provenance.
    Public,
    /// A loopback / RFC-1918 private / link-local / unspecified IP, or a
    /// `.onion` (Tor) or `localhost` host: reachable only inside a trust
    /// boundary the wider network cannot vouch for. A *discovered* relay
    /// naming one of these is an SSRF / forced-Tor vector; the engine admits
    /// it ONLY when a user explicitly opted that exact host in.
    Local,
}

/// Classify a relay URL by its HOST alone (issue #121). Pure — no DNS, no
/// I/O, no allocation beyond the domain lower-casing. The path, query, and
/// fragment are never consulted, so a public relay served at a per-user path
/// (`wss://nostr.wine/<npub>`) always classifies `Public`.
///
/// A URL with no host at all fails closed (`Local`): there is nothing public
/// to admit.
#[must_use]
pub fn classify_relay_host(url: &RelayUrl) -> RelayHostClass {
    match url.host() {
        Some(Host::Ipv4(ip)) => classify_ipv4(ip),
        Some(Host::Ipv6(ip)) => classify_ipv6(ip),
        Some(Host::Domain(name)) => classify_domain(name),
        None => RelayHostClass::Local,
    }
}

/// The HOST-only key the engine's opt-in allowlist matches against — the
/// SAME host component `classify_relay_host` keys on, normalized (lower-cased
/// domain / canonical IP text), never the scheme, port, or path. `None` for a
/// URL with no host. Two URLs to the same host on different ports share a
/// key: an operator opting `127.0.0.1` in trusts the HOST, per issue #121's
/// "user-configured local hosts" wording.
#[must_use]
pub fn relay_host_key(url: &RelayUrl) -> Option<String> {
    match url.host()? {
        Host::Domain(name) => Some(name.trim_end_matches('.').to_ascii_lowercase()),
        Host::Ipv4(ip) => Some(ip.to_string()),
        Host::Ipv6(ip) => Some(ip.to_string()),
    }
}

/// Classify a raw resolved `IpAddr` by the SAME local-range rules
/// [`classify_relay_host`] applies to a literal URL host (issue #519). This
/// is the post-DNS half of admission: a hostname's literal spelling can look
/// perfectly public while its A/AAAA answer still lands in a loopback,
/// RFC-1918, link-local, or IPv4-mapped-private range (an ordinary DNS-based
/// SSRF, or a rebind between resolution and connect). Callers that dial or
/// fetch by resolved address MUST run every candidate through this function
/// and refuse `Local` unless the ORIGINAL host was operator opted-in — see
/// `pool::connect::connect_with_timeout` (WS dial) and
/// `nmp-engine`'s `HickoryReqwestResolver::resolve` (NIP-11 fetch).
#[must_use]
pub fn classify_ip(ip: IpAddr) -> RelayHostClass {
    match ip {
        IpAddr::V4(ip) => classify_ipv4(ip),
        IpAddr::V6(ip) => classify_ipv6(ip),
    }
}

/// Normalize a bare HOST string (no scheme, no port, no path) to the same
/// canonical key [`relay_host_key`] derives from a parsed [`RelayUrl`]. Used
/// wherever a host needs to be checked against the operator's opt-in
/// allowlist without a full `RelayUrl` in hand: the WS dial's already-
/// resolved connect target (`pool::connect`), and the NIP-11 resolver's raw
/// DNS query name (`nmp-engine::relay_information`). An IP-literal host
/// keeps its canonical `Display` text (matching `Host::Ipv4`/`Host::Ipv6`'s
/// `to_string()` branch of `relay_host_key`); anything else is trimmed of a
/// trailing dot and lower-cased as an ordinary domain (matching
/// `Host::Domain`'s branch).
#[must_use]
pub fn normalize_bare_host(host: &str) -> String {
    let trimmed = host.trim_end_matches('.');
    match trimmed.parse::<IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => trimmed.to_ascii_lowercase(),
    }
}

fn classify_ipv4(ip: Ipv4Addr) -> RelayHostClass {
    if ip.is_loopback()        // 127.0.0.0/8
        || ip.is_private()     // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()  // 169.254.0.0/16
        || ip.is_unspecified() // 0.0.0.0
        || ip.is_broadcast()
    // 255.255.255.255
    {
        RelayHostClass::Local
    } else {
        RelayHostClass::Public
    }
}

fn classify_ipv6(ip: Ipv6Addr) -> RelayHostClass {
    let segs = ip.segments();
    // An IPv4-mapped (`::ffff:a.b.c.d`) or the deprecated IPv4-COMPATIBLE
    // (`::a.b.c.d`, RFC 4291 §2.5.5.1) address is really the v4 host it
    // embeds — classify by that so an embedded loopback/private/link-local
    // host is caught through the wrapper rather than passing as an
    // unremarkable public v6 address. `::` (unspecified) and `::1`
    // (loopback) technically fall in the compatible prefix too, but they are
    // their own well-known specials handled below, so they are excluded here.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return classify_ipv4(v4);
    }
    if segs[..6].iter().all(|&s| s == 0) && !ip.is_unspecified() && !ip.is_loopback() {
        let v4 = Ipv4Addr::new(
            (segs[6] >> 8) as u8,
            (segs[6] & 0xff) as u8,
            (segs[7] >> 8) as u8,
            (segs[7] & 0xff) as u8,
        );
        return classify_ipv4(v4);
    }
    let unique_local = (segs[0] & 0xfe00) == 0xfc00; // fc00::/7 (ULA)
    let link_local = (segs[0] & 0xffc0) == 0xfe80; // fe80::/10
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
        classify_relay_host(&RelayUrl::parse(url).expect("valid test relay url"))
    }

    /// The load-bearing falsifier for issue #121's "HOST, never path" rule:
    /// a real per-user relay served at a URL PATH must be admitted. If the
    /// classifier ever consulted the path this would regress.
    #[test]
    fn path_is_never_consulted_public_host_at_a_path_is_public() {
        assert_eq!(
            class("wss://nostr.wine/npub1xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"),
            RelayHostClass::Public,
            "a public host with a per-user path MUST pass admission — the path is not a host"
        );
    }

    #[test]
    fn ordinary_public_relays_are_public() {
        assert_eq!(class("wss://relay.damus.io"), RelayHostClass::Public);
        assert_eq!(class("wss://relay.example.com"), RelayHostClass::Public);
        assert_eq!(class("wss://nos.lol/"), RelayHostClass::Public);
        // A public host that merely LOOKS local by name is still public —
        // classification is on the parsed host, not a substring match.
        assert_eq!(class("wss://localhost.example.com"), RelayHostClass::Public);
    }

    #[test]
    fn ipv4_loopback_private_and_link_local_are_local() {
        assert_eq!(class("ws://127.0.0.1:7777"), RelayHostClass::Local);
        assert_eq!(class("ws://127.5.5.5"), RelayHostClass::Local);
        assert_eq!(class("ws://10.0.0.1"), RelayHostClass::Local);
        assert_eq!(class("ws://10.255.255.255"), RelayHostClass::Local);
        assert_eq!(class("ws://172.16.0.1"), RelayHostClass::Local);
        assert_eq!(class("ws://172.31.255.1"), RelayHostClass::Local);
        assert_eq!(class("ws://192.168.1.1"), RelayHostClass::Local);
        assert_eq!(class("ws://169.254.1.1"), RelayHostClass::Local);
        assert_eq!(class("ws://0.0.0.0"), RelayHostClass::Local);
        // The IPv4 limited-broadcast address: never a legitimate unicast
        // relay target, and reachable only on-segment.
        assert_eq!(class("ws://255.255.255.255"), RelayHostClass::Local);
    }

    /// A trailing-dot literal IPv4 host (`ws://127.0.0.1.`) must still
    /// classify `Local` — issue #519 Fix 3's falsifier. The `url` crate
    /// canonicalizes a dotted-quad host to `Host::Ipv4` regardless of a
    /// trailing root-zone dot, so this exercises the SAME `classify_ipv4`
    /// arm as the bare literal; pinned so a future parsing change that
    /// stopped canonicalizing the trailing dot would be caught immediately
    /// rather than silently reclassifying as an unremarkable `Domain`.
    #[test]
    fn trailing_dot_ipv4_literal_is_local() {
        assert_eq!(class("ws://127.0.0.1.:80"), RelayHostClass::Local);
        assert_eq!(class("ws://10.0.0.1."), RelayHostClass::Local);
    }

    /// Non-dotted IPv4 encodings (decimal `2130706433` and hex `0x7f000001`,
    /// both == 127.0.0.1) are canonicalized to an `Ipv4` host by the `url`
    /// crate for ws/wss URLs BEFORE we ever see them, so the classifier's
    /// IPv4 arm catches them for free. Pinned as a falsifier (not assumed):
    /// if a future `url`/`nostr` bump stopped canonicalizing these, they
    /// would arrive as `Domain` and silently classify `Public` — an SSRF
    /// bypass this test would immediately catch.
    #[test]
    fn non_dotted_ipv4_loopback_encodings_are_local() {
        assert_eq!(class("wss://2130706433"), RelayHostClass::Local);
        assert_eq!(class("wss://0x7f000001"), RelayHostClass::Local);
    }

    #[test]
    fn ipv4_public_ranges_stay_public() {
        // Just outside the RFC-1918 172.16/12 block.
        assert_eq!(class("ws://172.32.0.1"), RelayHostClass::Public);
        assert_eq!(class("ws://8.8.8.8"), RelayHostClass::Public);
        assert_eq!(class("ws://1.1.1.1"), RelayHostClass::Public);
    }

    #[test]
    fn ipv6_loopback_ula_link_local_and_mapped_are_local() {
        assert_eq!(class("ws://[::1]"), RelayHostClass::Local);
        assert_eq!(class("ws://[::]"), RelayHostClass::Local);
        assert_eq!(class("ws://[fc00::1]"), RelayHostClass::Local);
        assert_eq!(class("ws://[fd12:3456::1]"), RelayHostClass::Local);
        assert_eq!(class("ws://[fe80::1]"), RelayHostClass::Local);
        // IPv4-mapped loopback must be caught through the wrapper.
        assert_eq!(class("ws://[::ffff:127.0.0.1]"), RelayHostClass::Local);
        // The deprecated IPv4-COMPATIBLE loopback (`::127.0.0.1`, which the
        // url crate canonicalizes to `::7f00:1`) must ALSO be caught — a real
        // (if archaic) reachable loopback path.
        assert_eq!(class("ws://[::127.0.0.1]"), RelayHostClass::Local);
        assert_eq!(class("ws://[::7f00:1]"), RelayHostClass::Local);
        // ...and an IPv4-compatible RFC-1918 host, for good measure.
        assert_eq!(class("ws://[::0a00:0005]"), RelayHostClass::Local);
    }

    #[test]
    fn ipv6_public_stays_public() {
        assert_eq!(class("ws://[2606:4700:4700::1111]"), RelayHostClass::Public);
    }

    #[test]
    fn onion_and_localhost_hosts_are_local() {
        assert_eq!(
            class("ws://expyuzz4wqqyqhjn.onion"),
            RelayHostClass::Local,
            ".onion hosts silently force Tor and must not be admitted from discovery"
        );
        assert_eq!(class("ws://localhost:7777"), RelayHostClass::Local);
        assert_eq!(class("ws://LOCALHOST"), RelayHostClass::Local);
        assert_eq!(class("ws://foo.localhost"), RelayHostClass::Local);
    }

    #[test]
    fn host_key_is_host_only_and_port_insensitive() {
        let a = RelayUrl::parse("ws://127.0.0.1:7777").unwrap();
        let b = RelayUrl::parse("ws://127.0.0.1:9999/some/path").unwrap();
        assert_eq!(relay_host_key(&a), Some("127.0.0.1".to_string()));
        assert_eq!(
            relay_host_key(&a),
            relay_host_key(&b),
            "the allowlist key ignores port and path — it is the HOST"
        );
        assert_eq!(
            relay_host_key(&RelayUrl::parse("wss://Relay.Example.COM").unwrap()),
            Some("relay.example.com".to_string())
        );
    }

    /// [`classify_ip`] is the post-DNS half of admission (issue #519): it
    /// must agree exactly with [`classify_relay_host`]'s literal-IP verdict
    /// for every range that classifier already pins, since a resolved
    /// address in one of these ranges is exactly as dangerous as the same
    /// address spelled out in the URL.
    #[test]
    fn classify_ip_agrees_with_literal_host_classification() {
        use std::net::IpAddr;

        let local: &[&str] = &[
            "127.0.0.1",
            "127.5.5.5",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1", // link-local, incl. the cloud metadata endpoint
            "169.254.169.254",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "::ffff:127.0.0.1", // IPv4-mapped loopback
            "::ffff:10.0.0.1",  // IPv4-mapped private
        ];
        for host in local {
            let ip: IpAddr = host.parse().unwrap();
            assert_eq!(
                classify_ip(ip),
                RelayHostClass::Local,
                "resolved address {host} must classify Local"
            );
        }

        let public: &[&str] = &["8.8.8.8", "1.1.1.1", "172.32.0.1", "2606:4700:4700::1111"];
        for host in public {
            let ip: IpAddr = host.parse().unwrap();
            assert_eq!(
                classify_ip(ip),
                RelayHostClass::Public,
                "resolved address {host} must classify Public"
            );
        }
    }

    #[test]
    fn normalize_bare_host_matches_relay_host_key_conventions() {
        // IP literals keep their canonical text (same as `relay_host_key`'s
        // `Host::Ipv4`/`Host::Ipv6` branches — no case-folding applicable).
        assert_eq!(normalize_bare_host("127.0.0.1"), "127.0.0.1");
        // Domains lower-case and drop a trailing root-zone dot (same as
        // `relay_host_key`'s `Host::Domain` branch).
        assert_eq!(
            normalize_bare_host("Relay.Example.COM"),
            "relay.example.com"
        );
        assert_eq!(normalize_bare_host("relay.nmp.test."), "relay.nmp.test");
        assert_eq!(normalize_bare_host("LOCALHOST"), "localhost");
    }
}
