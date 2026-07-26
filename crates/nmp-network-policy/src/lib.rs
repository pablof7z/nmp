//! Pure network-destination admission.
//!
//! This crate owns only three facts:
//!
//! - canonical bare-host normalization;
//! - literal host/IP classification;
//! - whether one complete DNS answer set is admissible under an explicit
//!   local-host allowlist.
//!
//! It performs no DNS, socket, HTTP, WebSocket, relay, or protocol work.
//! Protocol adapters extract their own URL host, resolve or dial using their
//! own machinery, and may proceed only with the typed values returned here.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The security-relevant class of one literal host or resolved address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClass {
    Public,
    Local,
}

/// One canonical, engine-free destination policy.
///
/// Allowlist entries are bare host names or IP literals. They are normalized
/// once at construction, so literal-host and post-DNS checks cannot compare
/// different spellings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DestinationPolicy {
    allowed_local_hosts: BTreeSet<String>,
}

/// Proof that a literal host passed [`DestinationPolicy::admit_host`].
///
/// Its fields are private so post-DNS admission cannot be invoked with a host
/// token fabricated outside this crate.
///
/// The witness is the ONLY way into [`DestinationPolicy::admit_resolved`]:
///
/// ```
/// use nmp_network_policy::DestinationPolicy;
/// let policy = DestinationPolicy::default();
/// let host = policy.admit_host("relay.example.com").unwrap();
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
/// use nmp_network_policy::{AdmittedHost, DestinationPolicy};
/// let policy = DestinationPolicy::default();
/// let admitted = policy.admit_host("relay.example.com").unwrap();
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
    /// The literal URL host itself is local and was not explicitly allowed.
    LocalHostNotAllowed { host: String },
    /// At least one resolved address is local and the queried host was not
    /// explicitly allowed. `public_addresses` is nonempty for a mixed answer;
    /// that mixed set is refused in full.
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
                "destination host {host} is local and not operator allowed"
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
    /// Normalize and deduplicate the explicit local-host allowlist.
    #[must_use]
    pub fn new(allowed_local_hosts: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowed_local_hosts: allowed_local_hosts
                .into_iter()
                .map(|host| normalize_bare_host(&host))
                .filter(|host| !host.is_empty())
                .collect(),
        }
    }

    /// The exact normalized host keys owned by this policy.
    #[must_use]
    pub fn allowed_local_hosts(&self) -> &BTreeSet<String> {
        &self.allowed_local_hosts
    }

    /// Admit one bare host before resolution or socket work begins.
    ///
    /// Public-looking names are admitted provisionally but still receive only
    /// `PublicOnly` authority: their complete DNS answer must pass
    /// [`Self::admit_resolved`] before dialing.
    pub fn admit_host(&self, host: &str) -> Result<AdmittedHost, DestinationRefusal> {
        let key = normalize_bare_host(host);
        let explicitly_allowed = self.allowed_local_hosts.contains(&key);
        if classify_bare_host(&key) == HostClass::Local && !explicitly_allowed {
            return Err(DestinationRefusal::LocalHostNotAllowed { host: key });
        }
        Ok(AdmittedHost {
            key,
            local_resolution_authority: if explicitly_allowed {
                LocalResolutionAuthority::ExplicitlyAllowed
            } else {
                LocalResolutionAuthority::PublicOnly
            },
        })
    }

    /// Admit one complete DNS answer for an already-admitted literal host.
    ///
    /// An explicitly allowed host may resolve locally. Every other host is
    /// refused if even one answer is local; public answers do not launder a
    /// mixed set.
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
        Err(_) if normalized.ends_with(".onion") => HostClass::Local,
        Err(_) => HostClass::Public,
    }
}

/// Classify one literal or resolved IP address.
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
            "hiddenservice.onion",
        ] {
            assert_eq!(classify_bare_host(host), HostClass::Local, "{host}");
        }
        for host in [
            "8.8.8.8",
            "172.32.0.1",
            "2606:4700:4700::1111",
            "relay.example.com",
            "localhost.example.com",
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
        let policy = DestinationPolicy::new([
            " LOCALHOST. ".to_string(),
            "[::1]".to_string(),
            "Blossom.NMP.Test.".to_string(),
        ]);
        assert!(policy.admit_host("localhost").is_ok());
        assert!(policy.admit_host("LOCALHOST.").is_ok());
        assert!(policy.admit_host("::1").is_ok());
        assert!(policy.admit_host("[::1]").is_ok());
        assert!(policy.admit_host("blossom.nmp.test").is_ok());
        assert!(matches!(
            policy.admit_host("foo.localhost"),
            Err(DestinationRefusal::LocalHostNotAllowed { .. })
        ));
    }

    #[test]
    fn complete_resolved_set_is_admitted_or_refused_without_filtering() {
        let policy = DestinationPolicy::default();
        let host = policy.admit_host("relay.example.com").unwrap();
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
        let policy = DestinationPolicy::new(["relay.example.com".to_string()]);
        let host = policy.admit_host("RELAY.EXAMPLE.COM.").unwrap();
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

    #[test]
    fn empty_dns_answer_has_one_typed_refusal() {
        let policy = DestinationPolicy::default();
        let host = policy.admit_host("relay.example.com").unwrap();
        assert_eq!(
            policy.admit_resolved(&host, []),
            Err(DestinationRefusal::NoResolvedAddresses {
                host: "relay.example.com".to_string(),
            })
        );
    }
}
