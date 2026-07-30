//! Discovered-relay admission policy (issue #121): the provenance-aware half
//! of relay-URL admission.
//!
//! `nmp-network-policy` answers *what a host is* (public vs.
//! loopback/private/link-local/onion) and owns exact allowlist matching, with
//! no I/O. It deliberately stops there, because the SAFE answer depends on a
//! fact a pure destination policy does not have:
//! WHERE the URL came from. A `127.0.0.1` relay a user explicitly configured
//! for local development is fine; the same URL learned from untrusted network
//! content is an SSRF pivot. Protocol owners apply this policy before they
//! use the private neutral fact writer. Operator configuration is trusted and
//! bypasses this discovery gate.

use nmp_grammar::relay::relay_host_key;
use nmp_network_policy::DestinationPolicy;
use nmp_router::RelayUrl;

/// The operator's relay admission policy for DISCOVERED relays (issue #121).
///
/// Default (`RelayAdmissionPolicy::default()`) is the secure one: an empty
/// allowlist, so every discovered loopback/private/link-local/onion relay is
/// rejected. An operator opts specific local HOSTS back in (a dev relay on
/// `127.0.0.1`, a LAN relay) by listing them — matched by
/// [`nmp_grammar::relay::relay_host_key`], i.e. host-only, port- and
/// path-insensitive.
#[derive(Debug, Clone, Default)]
pub struct RelayAdmissionPolicy {
    /// The pure destination policy carrying the hosts a user EXPLICITLY
    /// opted in despite classifying `Local`. Empty by default → reject every
    /// discovered private/loopback/onion relay.
    destination_policy: DestinationPolicy,
}

impl RelayAdmissionPolicy {
    /// Build a policy from the operator's opt-in local HOST list. Each entry
    /// is normalized (trimmed, lower-cased) so it matches
    /// [`nmp_grammar::relay::relay_host_key`]'s canonical form. Accepts bare
    /// hosts (`"127.0.0.1"`, `"localhost"`); a full URL is reduced to its
    /// host if one is passed.
    #[must_use]
    pub fn new(allowed_local_hosts: impl IntoIterator<Item = String>) -> Self {
        let host_keys = allowed_local_hosts
            .into_iter()
            .map(|host| normalize_allow_entry(&host))
            .filter(|host| !host.is_empty());
        Self {
            destination_policy: DestinationPolicy::new(host_keys),
        }
    }

    /// The SAME pure destination policy this gate enforces at discovery time,
    /// so the transport pool's post-DNS-resolution answer-set check
    /// (`nmp_transport::PoolConfig::destination_policy`) and the NIP-11
    /// fetcher's resolver decide from one owner instead of re-deriving host
    /// normalization apiece (issue #519, #885). Both need to keep admitting
    /// an operator's INTENTIONAL local relay after its address is actually
    /// resolved, not only when its URL string is first classified.
    #[must_use]
    pub fn destination_policy(&self) -> &DestinationPolicy {
        &self.destination_policy
    }

    /// True iff a DISCOVERED relay at `url` may enter the routable directory:
    /// a public host always may; a `Local` host may ONLY if its host key was
    /// explicitly opted in.
    #[must_use]
    pub fn admits_discovered(&self, url: &RelayUrl) -> bool {
        relay_host_key(url).is_some_and(|host| self.destination_policy.admit_host(&host).is_ok())
    }
}

/// Reduce an operator allowlist entry to the host key it should match on.
/// A full URL (`ws://127.0.0.1:7777`) is parsed to its host; a bare host
/// (`127.0.0.1`, `localhost`) is used as-is after normalization.
fn normalize_allow_entry(entry: &str) -> String {
    let trimmed = entry.trim();
    if let Ok(url) = RelayUrl::parse(trimmed) {
        if let Some(key) = relay_host_key(&url) {
            return key;
        }
    }
    trimmed.trim_end_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_policy_rejects_every_discovered_local_host() {
        let policy = RelayAdmissionPolicy::default();
        assert!(!policy.admits_discovered(&RelayUrl::parse("ws://127.0.0.1:7777").unwrap()));
        assert!(!policy.admits_discovered(&RelayUrl::parse("ws://10.0.0.1").unwrap()));
        assert!(!policy.admits_discovered(&RelayUrl::parse("ws://192.168.1.5").unwrap()));
        assert!(!policy.admits_discovered(&RelayUrl::parse("ws://x.onion").unwrap()));
        assert!(!policy.admits_discovered(&RelayUrl::parse("ws://localhost").unwrap()));
    }

    #[test]
    fn default_policy_admits_public_hosts_including_a_per_user_path() {
        let policy = RelayAdmissionPolicy::default();
        assert!(policy.admits_discovered(&RelayUrl::parse("wss://relay.damus.io").unwrap()));
        assert!(policy.admits_discovered(&RelayUrl::parse("wss://nostr.wine/npub1abc").unwrap()));
    }

    #[test]
    fn opt_in_host_admits_that_discovered_local_relay_only() {
        let policy = RelayAdmissionPolicy::new(["127.0.0.1".to_string()]);
        // The opted-in host is admitted at any port / path.
        assert!(policy.admits_discovered(&RelayUrl::parse("ws://127.0.0.1:7777").unwrap()));
        assert!(policy.admits_discovered(&RelayUrl::parse("ws://127.0.0.1:9999/x").unwrap()));
        // A DIFFERENT local host is still rejected — the opt-in is exact.
        assert!(!policy.admits_discovered(&RelayUrl::parse("ws://10.0.0.1").unwrap()));
        assert!(!policy.admits_discovered(&RelayUrl::parse("ws://localhost").unwrap()));
    }

    #[test]
    fn opt_in_accepts_a_full_url_entry_and_matches_by_host() {
        let policy = RelayAdmissionPolicy::new(["ws://localhost:7777".to_string()]);
        assert!(policy.admits_discovered(&RelayUrl::parse("ws://localhost:8899").unwrap()));
    }

    #[test]
    fn filter_discovered_partitions_and_counts_rejections() {
        let policy = RelayAdmissionPolicy::default();
        let candidates = [
            "wss://relay.example.com",
            "ws://127.0.0.1:7777",
            "ws://10.0.0.9",
            "wss://nostr.wine/npub1abc",
        ]
        .into_iter()
        .map(|url| RelayUrl::parse(url).unwrap())
        .collect::<Vec<_>>();
        let admitted = candidates
            .iter()
            .filter(|relay| policy.admits_discovered(relay))
            .collect::<Vec<_>>();
        let rejected = candidates.len() - admitted.len();
        assert_eq!(rejected, 2, "the loopback and RFC-1918 relays are rejected");
        assert_eq!(admitted.len(), 2);
        assert!(admitted.iter().all(|relay| policy.admits_discovered(relay)));
    }
}
