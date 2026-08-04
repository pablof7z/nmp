//! Provenance-aware relay admission (issues #121, #1251).
//!
//! `nmp-network-policy` answers *what a host is* (public, local, `.onion`),
//! owns exact allowlist matching and the declared Tor capability, and does no
//! I/O. It cannot answer the question that actually decides admission, because
//! the answer is not a property of the address at all:
//!
//! **Whose declaration named this relay?**
//!
//! A `127.0.0.1` or `192.168.1.10` relay is meaningless — and possibly
//! hostile — in a stranger's data, and completely legitimate when this app's
//! operator or an identity it signs as named it, because they are describing
//! their own network. So this module owns the mapping from an NMP-internal
//! declaration site to [`Declarer`], and every relay URL that reaches routing
//! or a socket passes exactly one of them.
//!
//! Two properties this shape is designed to make unavailable:
//!
//! - There is no bag of "hosts somebody trusted once". A grant belongs to the
//!   exact declaration it came from, so heeding key B's own relay list can
//!   never widen what a write signing as key A is allowed to reach.
//! - There is no per-source filter to intersect. A relay named by any trusted
//!   tier is admitted even when an untrusted tier also names it, because the
//!   trusted tier already granted it.

use nmp_grammar::relay::relay_host_key;
use nmp_network_policy::{Declarer, DestinationPolicy, DestinationRefusal, OnionReachability};
use nmp_router::RelayUrl;

/// Where one relay URL came from, in NMP's own vocabulary, and therefore which
/// [`Declarer`] answers for it.
///
/// This enumerates declaration SITES, not trust levels: the whole point is that
/// each site has exactly one honest answer, decided once, here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelaySource {
    /// Anything the app handed the engine: `EngineConfig`'s indexer, app and
    /// fallback relays, `WriteRouting::Explicit`, `RelayScope::on`, a pinned
    /// read source, a relay the user typed into the app's own UI, and the URL
    /// passed to a one-shot relay-information request. The app named it for
    /// this exact piece of work.
    App,
    /// A relay list signed by an identity this engine can act as. Authorship,
    /// not arrival, is the test: a list that reached us by discovery from some
    /// stranger's relay is still ours if the key that signed it is ours.
    OwnIdentity,
    /// Anyone else's data: another author's relay list, a relay hint carried by
    /// a third party's `e`/`p`/`a` tag, a relay a row happened to arrive on.
    SomeoneElse,
}

impl RelaySource {
    fn declarer(self) -> Declarer {
        match self {
            Self::App | Self::OwnIdentity => Declarer::Ourselves,
            Self::SomeoneElse => Declarer::SomeoneElse,
        }
    }
}

/// The engine's relay admission policy.
///
/// The default is the secure one: no local host re-admitted, Tor not
/// reachable. Both knobs only ever affect [`RelaySource::SomeoneElse`] —
/// nothing an operator or an own identity declared has ever needed them.
#[derive(Debug, Clone, Default)]
pub struct RelayAdmissionPolicy {
    destination_policy: DestinationPolicy,
}

/// Why one relay URL was not admitted.
///
/// Carrying the reason is the difference between an app that can say "your
/// relay list names three relays and none of them were admitted, because they
/// are on your LAN and this app has not opted into local hosts" and one that
/// can only say "you have no relays".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayRefusal {
    /// The URL carries no host at all, so nothing can be classified.
    NoHost,
    /// The pure destination policy refused the host.
    Destination(DestinationRefusal),
}

impl std::fmt::Display for RelayRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHost => formatter.write_str("relay URL carries no host"),
            Self::Destination(refusal) => write!(formatter, "{refusal}"),
        }
    }
}

impl std::error::Error for RelayRefusal {}

impl RelayAdmissionPolicy {
    /// Build a policy from the operator's opt-in local HOST list and declared
    /// Tor reachability. Each host entry is normalized so it matches
    /// [`nmp_grammar::relay::relay_host_key`]'s canonical form; a full URL is
    /// reduced to its host if one is passed.
    #[must_use]
    pub fn new(
        allowed_local_hosts: impl IntoIterator<Item = String>,
        onion: OnionReachability,
    ) -> Self {
        let host_keys = allowed_local_hosts
            .into_iter()
            .map(|host| normalize_allow_entry(&host))
            .filter(|host| !host.is_empty());
        Self {
            destination_policy: DestinationPolicy::new(host_keys, onion),
        }
    }

    /// The SAME pure destination policy this gate enforces, so the transport
    /// pool's post-DNS-resolution answer-set check
    /// (`nmp_transport::PoolConfig::destination_policy`) and the NIP-11
    /// fetcher's resolver decide from one owner instead of re-deriving host
    /// normalization apiece (issues #519, #885). One owner is the point: a
    /// routing layer that admits what the socket layer refuses is two owners
    /// of one property, and the provenance answer has to survive to the dial.
    #[must_use]
    pub fn destination_policy(&self) -> &DestinationPolicy {
        &self.destination_policy
    }

    /// Whether a relay declared by `source` may be used, and why not if not.
    pub fn admits(&self, url: &RelayUrl, source: RelaySource) -> Result<(), RelayRefusal> {
        let Some(host) = relay_host_key(url) else {
            return Err(RelayRefusal::NoHost);
        };
        self.destination_policy
            .admit_host(&host, source.declarer())
            .map(|_| ())
            .map_err(RelayRefusal::Destination)
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

    fn relay(url: &str) -> RelayUrl {
        RelayUrl::parse(url).expect("valid test relay url")
    }

    /// The headline rule, as one table: the SAME four URLs, refused as a
    /// stranger's claim and heeded as our own declaration.
    #[test]
    fn the_same_relay_is_refused_from_a_stranger_and_heeded_from_us() {
        let policy = RelayAdmissionPolicy::default();
        for url in [
            "ws://127.0.0.1:7777",
            "ws://10.0.0.1",
            "ws://192.168.1.5",
            "ws://localhost",
        ] {
            let url = relay(url);
            assert!(
                policy.admits(&url, RelaySource::SomeoneElse).is_err(),
                "someone else's data may not name {url}"
            );
            assert!(
                policy.admits(&url, RelaySource::OwnIdentity).is_ok(),
                "our own signed relay list describes our own network: {url}"
            );
            assert!(
                policy.admits(&url, RelaySource::App).is_ok(),
                "the app naming it is the operator describing their own network: {url}"
            );
        }
    }

    #[test]
    fn public_hosts_need_no_grant_from_anyone() {
        let policy = RelayAdmissionPolicy::default();
        for url in ["wss://relay.damus.io", "wss://nostr.wine/npub1abc"] {
            assert!(policy.admits(&relay(url), RelaySource::SomeoneElse).is_ok());
        }
    }

    #[test]
    fn the_operator_opt_in_readmits_that_exact_host_from_anyone() {
        let policy =
            RelayAdmissionPolicy::new(["127.0.0.1".to_string()], OnionReachability::Unreachable);
        // The opted-in host is admitted at any port / path.
        assert!(policy
            .admits(&relay("ws://127.0.0.1:7777"), RelaySource::SomeoneElse)
            .is_ok());
        assert!(policy
            .admits(&relay("ws://127.0.0.1:9999/x"), RelaySource::SomeoneElse)
            .is_ok());
        // A DIFFERENT local host is still refused — the opt-in is exact.
        assert!(policy
            .admits(&relay("ws://10.0.0.1"), RelaySource::SomeoneElse)
            .is_err());
        assert!(policy
            .admits(&relay("ws://localhost"), RelaySource::SomeoneElse)
            .is_err());
    }

    #[test]
    fn opt_in_accepts_a_full_url_entry_and_matches_by_host() {
        let policy = RelayAdmissionPolicy::new(
            ["ws://localhost:7777".to_string()],
            OnionReachability::Unreachable,
        );
        assert!(policy
            .admits(&relay("ws://localhost:8899"), RelaySource::SomeoneElse)
            .is_ok());
    }

    /// Tor is a reachability declaration, not a local-host grant, and it is
    /// what makes OTHER people's hidden services usable.
    #[test]
    fn declared_tor_reachability_admits_another_persons_onion_relay() {
        let stranger_onion = relay("ws://nmprelayxyz.onion");

        let without_tor = RelayAdmissionPolicy::default();
        assert!(without_tor
            .admits(&stranger_onion, RelaySource::SomeoneElse)
            .is_err());
        assert!(
            without_tor
                .admits(&stranger_onion, RelaySource::OwnIdentity)
                .is_ok(),
            "our own list naming a hidden service is heeded even with no Tor; \
             it just fails to connect"
        );

        let with_tor = RelayAdmissionPolicy::new([], OnionReachability::Reachable);
        assert!(with_tor
            .admits(&stranger_onion, RelaySource::SomeoneElse)
            .is_ok());
        assert!(
            with_tor
                .admits(&relay("ws://127.0.0.1:7777"), RelaySource::SomeoneElse)
                .is_err(),
            "declaring Tor must not quietly re-admit loopback from strangers"
        );
    }

    #[test]
    fn a_refusal_carries_the_reason_the_app_has_to_show_a_user() {
        let policy = RelayAdmissionPolicy::default();
        let refusal = policy
            .admits(&relay("ws://192.168.1.5"), RelaySource::SomeoneElse)
            .unwrap_err();
        assert!(
            refusal.to_string().contains("192.168.1.5"),
            "the reason names the exact host: {refusal}"
        );
        let refusal = policy
            .admits(&relay("ws://nmprelayxyz.onion"), RelaySource::SomeoneElse)
            .unwrap_err();
        assert!(
            matches!(
                refusal,
                RelayRefusal::Destination(DestinationRefusal::OnionUnreachable { .. })
            ),
            "an unreachable hidden service is its own reason, not 'local host': {refusal}"
        );
    }
}
