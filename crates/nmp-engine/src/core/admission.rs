//! Discovered-relay admission policy (issue #121): the provenance-aware half
//! of relay-URL admission.
//!
//! `nmp-transport::classify_relay_host` answers *what a host is* (public vs.
//! loopback/private/link-local/onion) with no I/O. It deliberately stops
//! there, because the SAFE answer depends on a fact transport does not have:
//! WHERE the URL came from. A `127.0.0.1` relay a user explicitly configured
//! for local development is fine; the SAME `127.0.0.1` arriving inside a
//! network-sourced, validly-signed kind:10002 is an SSRF pivot. Provenance is
//! the whole difference, so the decision lives here in the engine — the layer
//! that knows a URL was *discovered* — never in transport.
//!
//! This policy is applied at exactly one choke point: `EngineCore::
//! ingest_relay_list_winner`, where a kind:10002 winner's parsed `r`-tag
//! relays are about to become routable `Nip65Write`/`Nip65Read` lanes in the
//! directory. A relay rejected here never enters the directory, so the router
//! never builds a candidate for it, so no `Effect` ever names it, so it never
//! reaches `pool.ensure_open`. Rejection is structural, not a downstream
//! filter (bug-class-ledger method: make the bad state unreachable, then
//! prove it with one falsifier).
//!
//! Relays that enter through operator config (`LiveDirectory`'s
//! indexers/app/fallback builder inputs) never pass through this gate — that
//! is the intended provenance split: config is trusted, discovery is not.

use std::collections::BTreeSet;

use nmp_router::{LanedRelay, RelayUrl};
use nmp_transport::{classify_relay_host, relay_host_key, RelayHostClass};

/// The operator's relay admission policy for DISCOVERED relays (issue #121).
///
/// Default (`RelayAdmissionPolicy::default()`) is the secure one: an empty
/// allowlist, so every discovered loopback/private/link-local/onion relay is
/// rejected. An operator opts specific local HOSTS back in (a dev relay on
/// `127.0.0.1`, a LAN relay) by listing them — matched by
/// [`nmp_transport::relay_host_key`], i.e. host-only, port- and
/// path-insensitive.
#[derive(Debug, Clone, Default)]
pub struct RelayAdmissionPolicy {
    /// Host keys a user EXPLICITLY opted in despite classifying `Local`.
    /// Empty by default → reject every discovered private/loopback/onion
    /// relay.
    allowed_local_hosts: BTreeSet<String>,
}

impl RelayAdmissionPolicy {
    /// Build a policy from the operator's opt-in local HOST list. Each entry
    /// is normalized (trimmed, lower-cased) so it matches
    /// [`nmp_transport::relay_host_key`]'s canonical form. Accepts bare hosts
    /// (`"127.0.0.1"`, `"localhost"`); a full URL is reduced to its host if
    /// one is passed.
    #[must_use]
    pub fn new(allowed_local_hosts: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowed_local_hosts: allowed_local_hosts
                .into_iter()
                .map(|h| normalize_allow_entry(&h))
                .filter(|h| !h.is_empty())
                .collect(),
        }
    }

    /// True iff a DISCOVERED relay at `url` may enter the routable directory:
    /// a public host always may; a `Local` host may ONLY if its host key was
    /// explicitly opted in.
    #[must_use]
    pub fn admits_discovered(&self, url: &RelayUrl) -> bool {
        match classify_relay_host(url) {
            RelayHostClass::Public => true,
            RelayHostClass::Local => {
                relay_host_key(url).is_some_and(|h| self.allowed_local_hosts.contains(&h))
            }
        }
    }

    /// Split a discovered lane's relays into the admitted set and the count
    /// rejected. The count feeds `EngineCore`'s diagnostics rejection tally
    /// (issue #121: "count rejections in diagnostics").
    #[must_use]
    pub fn filter_discovered(&self, relays: Vec<LanedRelay>) -> (Vec<LanedRelay>, u64) {
        let mut rejected = 0u64;
        let admitted = relays
            .into_iter()
            .filter(|r| {
                let ok = self.admits_discovered(&r.url);
                if !ok {
                    rejected += 1;
                }
                ok
            })
            .collect();
        (admitted, rejected)
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
    use nmp_router::Lane;

    fn laned(url: &str) -> LanedRelay {
        LanedRelay::new(RelayUrl::parse(url).unwrap(), Lane::Nip65Write)
    }

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
        let (admitted, rejected) = policy.filter_discovered(vec![
            laned("wss://relay.example.com"),
            laned("ws://127.0.0.1:7777"),
            laned("ws://10.0.0.9"),
            laned("wss://nostr.wine/npub1abc"),
        ]);
        assert_eq!(rejected, 2, "the loopback and RFC-1918 relays are rejected");
        assert_eq!(admitted.len(), 2);
        assert!(admitted.iter().all(|r| policy.admits_discovered(&r.url)));
    }
}
