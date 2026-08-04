//! Pure network-destination admission.
//!
//! This crate owns only four facts:
//!
//! - canonical bare-host normalization;
//! - literal host/IP classification;
//! - whether a destination is admissible given WHOSE declaration named it;
//! - whether one complete DNS answer set is admissible for a host already
//!   admitted.
//!
//! It performs no DNS, socket, HTTP, WebSocket, relay, or protocol work.
//! Protocol adapters extract their own URL host, resolve or dial using their
//! own machinery, and may proceed only with the typed values returned here.
//!
//! # The two independent questions
//!
//! Admission used to be one list of address shapes nobody may dial. It is
//! really two questions that happen to be asked at the same moment:
//!
//! 1. **Whose declaration is this?** ([`Declarer`]) A loopback or RFC-1918
//!    address is meaningless — and possibly hostile — in somebody else's data,
//!    and entirely legitimate when this app's operator or an identity it signs
//!    as named it, because they are describing their own network. Address
//!    shape decides nothing on its own; provenance does.
//! 2. **Can this process reach a `.onion` name at all?**
//!    ([`OnionReachability`]) That is not a "my network" question, so it is not
//!    on the provenance axis. An app that has arranged Tor reachability says
//!    so once, and then other people's `.onion` relays become usable — not
//!    only its own.
//!
//! Admitting is permission to try, never a promise the destination works: an
//! own declaration naming a `.onion` with no Tor available is admitted here
//! and simply fails to connect.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The security-relevant class of one literal host or resolved address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClass {
    Public,
    /// Loopback, RFC-1918, link-local, unspecified, or broadcast: an address
    /// that means something only on somebody's own network.
    Local,
    /// A Tor hidden-service name. Not local, not public, and not resolvable
    /// without Tor — a reachability question rather than a network-ownership
    /// one, which is why it is its own class.
    Onion,
}

/// Whose declaration named this destination.
///
/// This is the whole of the provenance axis. It is deliberately not a
/// confidence score or a source enumeration: every trusted tier collapses to
/// the same answer, and a destination named by any trusted tier is admitted
/// even when an untrusted tier also names it — admission is a union of grants,
/// never a per-source filter that gets intersected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Declarer {
    /// This app's operator, or an identity this app can sign as, named this
    /// destination. They are describing their own network, so every address
    /// shape they name is heeded.
    Ourselves,
    /// Someone else's data named it: another author's list, a relay hint in a
    /// third party's event, anything unverified. Address shape now decides.
    SomeoneElse,
}

/// Whether this process can reach a Tor hidden service.
///
/// The app declares it; NMP never probes for it. Declaring reachability does
/// not install a Tor transport — it states that one exists, and admission
/// stops standing in the way of `.onion` destinations nobody in this process
/// declared themselves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OnionReachability {
    /// No Tor. A `.onion` name someone else declared is refused before any
    /// socket work; one we declared ourselves is still admitted and simply
    /// fails to connect.
    #[default]
    Unreachable,
    /// Tor is available to this process, so `.onion` destinations are ordinary
    /// destinations regardless of who named them.
    Reachable,
}

/// One canonical, engine-free destination policy.
///
/// Allowlist entries are bare host names or IP literals. They are normalized
/// once at construction, so literal-host and post-DNS checks cannot compare
/// different spellings.
///
/// The allowlist re-admits LOCAL hosts named by someone else. It says nothing
/// about `.onion`, which [`OnionReachability`] owns, and nothing about hosts we
/// declared ourselves, which [`Declarer::Ourselves`] already admits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DestinationPolicy {
    allowed_local_hosts: BTreeSet<String>,
    onion: OnionReachability,
}

/// Proof that a literal host passed [`DestinationPolicy::admit_host`].
///
/// Its fields are private so post-DNS admission cannot be invoked with a host
/// token fabricated outside this crate.
///
/// The witness is the ONLY way into [`DestinationPolicy::admit_resolved`]:
///
/// ```
/// use nmp_network_policy::{Declarer, DestinationPolicy};
/// let policy = DestinationPolicy::default();
/// let host = policy
///     .admit_host("relay.example.com", Declarer::SomeoneElse)
///     .unwrap();
/// assert!(policy.admit_resolved(&host, ["8.8.8.8".parse().unwrap()]).is_ok());
/// ```
///
/// A fabricated witness does not compile — the fields are private, so no
/// caller outside this crate can mint one and skip the literal-host check:
///
/// ```compile_fail
/// use nmp_network_policy::{AdmittedHost, DestinationPolicy};
/// let policy = DestinationPolicy::default();
/// let forged = AdmittedHost {
///     key: "127.0.0.1".to_string(),
///     local_resolution_authority: (),
/// };
/// let _ = policy.admit_resolved(&forged, ["127.0.0.1".parse().unwrap()]);
/// ```
///
/// Nor can one be assembled from a value the crate did hand out, because
/// there is no public constructor, no public field, and no `From`/`Default`
/// path to the authority discriminant:
///
/// ```compile_fail
/// use nmp_network_policy::{AdmittedHost, Declarer, DestinationPolicy};
/// let policy = DestinationPolicy::default();
/// let admitted = policy
///     .admit_host("relay.example.com", Declarer::SomeoneElse)
///     .unwrap();
/// let escalated = AdmittedHost {
///     key: "127.0.0.1".to_string(),
///     ..admitted
/// };
/// let _ = policy.admit_resolved(&escalated, ["127.0.0.1".parse().unwrap()]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedHost {
    key: String,
    local_resolution_authority: LocalResolutionAuthority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalResolutionAuthority {
    PublicOnly,
    ExplicitlyAllowed,
}

/// One complete DNS answer set admitted without filtering or widening.
///
/// Mixed public/local answers are never reduced to the public subset. The
/// whole set is either admitted or refused, so callers cannot silently weaken
/// rebinding protection by selecting only convenient answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedAddresses(Vec<IpAddr>);

/// Typed reasons a destination cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestinationRefusal {
    /// Someone else's data named a local literal host, and no operator
    /// allowlist entry re-admits it.
    LocalHostNotAllowed { host: String },
    /// Someone else's data named a Tor hidden service and this process has not
    /// declared Tor reachable.
    OnionUnreachable { host: String },
    /// At least one resolved address is local and the queried host had no
    /// local-resolution authority. `public_addresses` is nonempty for a mixed
    /// answer; that mixed set is refused in full.
    ResolvedLocalAddressesNotAllowed {
        host: String,
        local_addresses: Vec<IpAddr>,
        public_addresses: Vec<IpAddr>,
    },
    /// DNS returned no connection destination.
    NoResolvedAddresses { host: String },
}

impl std::fmt::Display for DestinationRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocalHostNotAllowed { host } => write!(
                formatter,
                "destination host {host} is local, was declared by someone else, \
                 and is not operator allowed"
            ),
            Self::OnionUnreachable { host } => write!(
                formatter,
                "destination host {host} was declared by someone else and this \
                 process has not declared Tor reachable"
            ),
            Self::ResolvedLocalAddressesNotAllowed {
                host,
                local_addresses,
                public_addresses,
            } if public_addresses.is_empty() => write!(
                formatter,
                "destination host {host} resolved only to unallowed local addresses \
                 {local_addresses:?}"
            ),
            Self::ResolvedLocalAddressesNotAllowed {
                host,
                local_addresses,
                public_addresses,
            } => write!(
                formatter,
                "destination host {host} returned a mixed DNS answer; local addresses \
                 {local_addresses:?} make the complete set containing public addresses \
                 {public_addresses:?} inadmissible"
            ),
            Self::NoResolvedAddresses { host } => {
                write!(
                    formatter,
                    "destination host {host} resolved to no addresses"
                )
            }
        }
    }
}

impl std::error::Error for DestinationRefusal {}

impl DestinationPolicy {
    /// Normalize and deduplicate the explicit local-host allowlist, and record
    /// whether this process can reach a Tor hidden service.
    #[must_use]
    pub fn new(
        allowed_local_hosts: impl IntoIterator<Item = String>,
        onion: OnionReachability,
    ) -> Self {
        Self {
            allowed_local_hosts: allowed_local_hosts
                .into_iter()
                .map(|host| normalize_bare_host(&host))
                .filter(|host| !host.is_empty())
                .collect(),
            onion,
        }
    }

    /// The exact normalized host keys owned by this policy.
    #[must_use]
    pub fn allowed_local_hosts(&self) -> &BTreeSet<String> {
        &self.allowed_local_hosts
    }

    /// Whether this process declared Tor reachable.
    #[must_use]
    pub fn onion_reachability(&self) -> OnionReachability {
        self.onion
    }

    /// Admit one bare host before resolution or socket work begins.
    ///
    /// A host we declared ourselves is admitted whatever its shape, and
    /// carries local-resolution authority: describing your own network is
    /// exactly the case where a name legitimately resolves to an address only
    /// you can reach.
    ///
    /// A host someone else declared is admitted when it is public, when it is
    /// local and the operator allowlist names it, or when it is `.onion` and
    /// Tor is declared reachable. Public-looking names still receive only
    /// `PublicOnly` authority: their complete DNS answer must pass
    /// [`Self::admit_resolved`] before dialing.
    pub fn admit_host(
        &self,
        host: &str,
        declarer: Declarer,
    ) -> Result<AdmittedHost, DestinationRefusal> {
        let key = normalize_bare_host(host);
        if declarer == Declarer::Ourselves {
            return Ok(AdmittedHost {
                key,
                local_resolution_authority: LocalResolutionAuthority::ExplicitlyAllowed,
            });
        }
        match classify_bare_host(&key) {
            // `.onion` is off the local-host axis entirely: only declared
            // reachability answers for it, and the allowlist grants it
            // nothing. A resolved hidden-service answer is whatever the local
            // Tor resolver hands back — routinely a loopback or otherwise
            // private address — so reachability necessarily carries
            // local-resolution authority.
            HostClass::Onion if self.onion == OnionReachability::Reachable => Ok(AdmittedHost {
                key,
                local_resolution_authority: LocalResolutionAuthority::ExplicitlyAllowed,
            }),
            HostClass::Onion => Err(DestinationRefusal::OnionUnreachable { host: key }),
            // Naming a host in the allowlist IS the operator declaring it, so
            // it carries local-resolution authority whatever the literal name
            // looks like: the dev relay that answers `relay.example.com` with
            // `127.0.0.1` is the case this opt-in exists for.
            _ if self.allowed_local_hosts.contains(&key) => Ok(AdmittedHost {
                key,
                local_resolution_authority: LocalResolutionAuthority::ExplicitlyAllowed,
            }),
            HostClass::Public => Ok(AdmittedHost {
                key,
                local_resolution_authority: LocalResolutionAuthority::PublicOnly,
            }),
            HostClass::Local => Err(DestinationRefusal::LocalHostNotAllowed { host: key }),
        }
    }

    /// Admit one complete DNS answer for an already-admitted literal host.
    ///
    /// A host with local-resolution authority may resolve locally. Every other
    /// host is refused if even one answer is local; public answers do not
    /// launder a mixed set.
    pub fn admit_resolved(
        &self,
        host: &AdmittedHost,
        addresses: impl IntoIterator<Item = IpAddr>,
    ) -> Result<AdmittedAddresses, DestinationRefusal> {
        let addresses: Vec<IpAddr> = addresses.into_iter().collect();
        if addresses.is_empty() {
            return Err(DestinationRefusal::NoResolvedAddresses {
                host: host.key.clone(),
            });
        }
        if host.local_resolution_authority == LocalResolutionAuthority::ExplicitlyAllowed {
            return Ok(AdmittedAddresses(addresses));
        }

        let (local_addresses, public_addresses): (Vec<_>, Vec<_>) = addresses
            .iter()
            .copied()
            .partition(|address| classify_ip(*address) == HostClass::Local);
        if !local_addresses.is_empty() {
            return Err(DestinationRefusal::ResolvedLocalAddressesNotAllowed {
                host: host.key.clone(),
                local_addresses,
                public_addresses,
            });
        }
        Ok(AdmittedAddresses(addresses))
    }
}

impl AdmittedHost {
    /// The canonical host key used for policy matching and diagnostics.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl AdmittedAddresses {
    /// Iterate the exact, complete admitted answer set.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &IpAddr> {
        self.0.iter()
    }

    /// Consume the proof and recover the exact, complete admitted answer set.
    #[must_use]
    pub fn into_vec(self) -> Vec<IpAddr> {
        self.0
    }
}

/// Normalize a bare host for classification and allowlist comparison.
///
/// URL parsing and host extraction remain protocol-adapter responsibilities.
#[must_use]
pub fn normalize_bare_host(host: &str) -> String {
    let trimmed = host.trim();
    let bare = trimmed
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(trimmed)
        .trim_end_matches('.');
    match bare.parse::<IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => bare.to_ascii_lowercase(),
    }
}

/// Classify one already-extracted bare host.
#[must_use]
pub fn classify_bare_host(host: &str) -> HostClass {
    let normalized = normalize_bare_host(host);
    match normalized.parse::<IpAddr>() {
        Ok(ip) => classify_ip(ip),
        Err(_) if normalized == "localhost" => HostClass::Local,
        Err(_) if normalized.ends_with(".localhost") => HostClass::Local,
        Err(_) if normalized == "onion" || normalized.ends_with(".onion") => HostClass::Onion,
        Err(_) => HostClass::Public,
    }
}

/// Classify one literal or resolved IP address.
///
/// An IP is never [`HostClass::Onion`]: a hidden service is a name, and by the
/// time an address exists Tor has already answered for it.
#[must_use]
pub fn classify_ip(ip: IpAddr) -> HostClass {
    match ip {
        IpAddr::V4(ip) => classify_ipv4(ip),
        IpAddr::V6(ip) => classify_ipv6(ip),
    }
}

fn classify_ipv4(ip: Ipv4Addr) -> HostClass {
    if ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
    {
        HostClass::Local
    } else {
        HostClass::Public
    }
}

fn classify_ipv6(ip: Ipv6Addr) -> HostClass {
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
        HostClass::Local
    } else {
        HostClass::Public
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_host_table_has_one_canonical_classification() {
        for host in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "[::1]",
            "::",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::0a00:0005",
            "localhost",
            "LOCALHOST.",
            "foo.localhost",
        ] {
            assert_eq!(classify_bare_host(host), HostClass::Local, "{host}");
        }
        for host in ["hiddenservice.onion", "HiddenService.Onion."] {
            assert_eq!(classify_bare_host(host), HostClass::Onion, "{host}");
        }
        for host in [
            "8.8.8.8",
            "172.32.0.1",
            "2606:4700:4700::1111",
            "relay.example.com",
            "localhost.example.com",
            "onion.example.com",
        ] {
            assert_eq!(classify_bare_host(host), HostClass::Public, "{host}");
        }
    }

    #[test]
    fn resolved_addresses_use_the_same_ranges_as_literal_hosts() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
        ] {
            assert_eq!(
                classify_ip(address.parse().unwrap()),
                HostClass::Local,
                "{address}"
            );
        }
        for address in ["8.8.8.8", "172.32.0.1", "2606:4700:4700::1111"] {
            assert_eq!(
                classify_ip(address.parse().unwrap()),
                HostClass::Public,
                "{address}"
            );
        }
    }

    #[test]
    fn normalization_and_explicit_allowlist_matching_cannot_drift() {
        let policy = DestinationPolicy::new(
            [
                " LOCALHOST. ".to_string(),
                "[::1]".to_string(),
                "Blossom.NMP.Test.".to_string(),
            ],
            OnionReachability::Unreachable,
        );
        for host in [
            "localhost",
            "LOCALHOST.",
            "::1",
            "[::1]",
            "blossom.nmp.test",
        ] {
            assert!(
                policy.admit_host(host, Declarer::SomeoneElse).is_ok(),
                "{host}"
            );
        }
        assert!(matches!(
            policy.admit_host("foo.localhost", Declarer::SomeoneElse),
            Err(DestinationRefusal::LocalHostNotAllowed { .. })
        ));
    }

    /// The whole point of the provenance axis: the address decides nothing on
    /// its own, and the SAME address gets opposite answers depending on whose
    /// declaration named it.
    #[test]
    fn one_address_gets_opposite_answers_from_the_two_declarers() {
        let policy = DestinationPolicy::default();
        for host in ["127.0.0.1", "192.168.1.10", "localhost", "fe80::1"] {
            assert!(
                policy.admit_host(host, Declarer::Ourselves).is_ok(),
                "our own declaration describes our own network: {host}"
            );
            assert!(
                policy.admit_host(host, Declarer::SomeoneElse).is_err(),
                "the same address in someone else's data is refused: {host}"
            );
        }
    }

    /// Heeding is permission to try, not a promise it works: an own `.onion`
    /// is admitted with no Tor at all, and simply fails later.
    #[test]
    fn onion_is_governed_by_reachability_not_by_the_local_allowlist() {
        let without_tor = DestinationPolicy::default();
        assert!(without_tor
            .admit_host("abcdef.onion", Declarer::Ourselves)
            .is_ok());
        assert_eq!(
            without_tor.admit_host("abcdef.onion", Declarer::SomeoneElse),
            Err(DestinationRefusal::OnionUnreachable {
                host: "abcdef.onion".to_string()
            })
        );

        let with_tor = DestinationPolicy::new([], OnionReachability::Reachable);
        assert!(
            with_tor
                .admit_host("abcdef.onion", Declarer::SomeoneElse)
                .is_ok(),
            "a declared Tor capability admits OTHER people's hidden services"
        );

        // The local-host allowlist is about local hosts. Listing a hidden
        // service there grants nothing, because `.onion` is not on that axis.
        let allowlisted =
            DestinationPolicy::new(["abcdef.onion".to_string()], OnionReachability::Unreachable);
        assert_eq!(
            allowlisted.admit_host("abcdef.onion", Declarer::SomeoneElse),
            Err(DestinationRefusal::OnionUnreachable {
                host: "abcdef.onion".to_string()
            })
        );
    }

    /// Tor reachability is not a local-host grant: declaring Tor must not
    /// quietly re-admit loopback or RFC-1918 from someone else's data.
    #[test]
    fn tor_reachability_grants_nothing_to_local_addresses() {
        let policy = DestinationPolicy::new([], OnionReachability::Reachable);
        assert!(matches!(
            policy.admit_host("127.0.0.1", Declarer::SomeoneElse),
            Err(DestinationRefusal::LocalHostNotAllowed { .. })
        ));
        assert!(matches!(
            policy.admit_host("192.168.1.10", Declarer::SomeoneElse),
            Err(DestinationRefusal::LocalHostNotAllowed { .. })
        ));
    }

    #[test]
    fn complete_resolved_set_is_admitted_or_refused_without_filtering() {
        let policy = DestinationPolicy::default();
        let host = policy
            .admit_host("relay.example.com", Declarer::SomeoneElse)
            .unwrap();
        let public: Vec<IpAddr> = ["8.8.8.8", "2606:4700:4700::1111"]
            .into_iter()
            .map(|address| address.parse().unwrap())
            .collect();
        assert_eq!(
            policy
                .admit_resolved(&host, public.clone())
                .unwrap()
                .into_vec(),
            public
        );

        let local: IpAddr = "127.0.0.1".parse().unwrap();
        let public: IpAddr = "8.8.8.8".parse().unwrap();
        assert_eq!(
            policy.admit_resolved(&host, [public, local]),
            Err(DestinationRefusal::ResolvedLocalAddressesNotAllowed {
                host: "relay.example.com".to_string(),
                local_addresses: vec![local],
                public_addresses: vec![public],
            }),
            "one local answer refuses the entire mixed set"
        );
    }

    #[test]
    fn explicit_host_authority_admits_local_and_mixed_answers_exactly() {
        let policy = DestinationPolicy::new(
            ["relay.example.com".to_string()],
            OnionReachability::Unreachable,
        );
        let host = policy
            .admit_host("RELAY.EXAMPLE.COM.", Declarer::SomeoneElse)
            .unwrap();
        let addresses: Vec<IpAddr> = ["127.0.0.1", "8.8.8.8"]
            .into_iter()
            .map(|address| address.parse().unwrap())
            .collect();
        assert_eq!(
            policy
                .admit_resolved(&host, addresses.clone())
                .unwrap()
                .into_vec(),
            addresses
        );
    }

    /// A public-looking name we declared ourselves that resolves to our own
    /// LAN is the ordinary "my relay is at home" shape, not a rebinding
    /// attack, so the same authority has to survive to the post-DNS check.
    #[test]
    fn our_own_public_looking_host_may_resolve_onto_our_own_network() {
        let policy = DestinationPolicy::default();
        let ours = policy
            .admit_host("relay.example.com", Declarer::Ourselves)
            .unwrap();
        let theirs = policy
            .admit_host("relay.example.com", Declarer::SomeoneElse)
            .unwrap();
        let answer: IpAddr = "192.168.1.10".parse().unwrap();
        assert!(policy.admit_resolved(&ours, [answer]).is_ok());
        assert!(matches!(
            policy.admit_resolved(&theirs, [answer]),
            Err(DestinationRefusal::ResolvedLocalAddressesNotAllowed { .. })
        ));
    }

    #[test]
    fn empty_dns_answer_has_one_typed_refusal() {
        let policy = DestinationPolicy::default();
        let host = policy
            .admit_host("relay.example.com", Declarer::SomeoneElse)
            .unwrap();
        assert_eq!(
            policy.admit_resolved(&host, []),
            Err(DestinationRefusal::NoResolvedAddresses {
                host: "relay.example.com".to_string(),
            })
        );
    }
}
