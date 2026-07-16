//! BUD-02 upload client (#545): an async, sha256-self-verifying `PUT
//! /upload` with the SAME HTTP admission discipline as the engine's NIP-11
//! fetcher (`nmp-engine/src/relay_information.rs`, issue #519): literal
//! loopback/private/link-local/onion hosts are refused BEFORE any socket
//! I/O unless operator opted-in, resolved DNS answers are filtered through
//! `nmp_transport::classify_ip` (failing closed when every answer is
//! local), redirects/proxies/referrers/retries are disabled, and the
//! response body is read streamed under a byte cap. The engine's private
//! helpers are reimplemented here from `nmp-transport`'s PUBLIC pure
//! classifiers because this crate must stay engine-free.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use nmp_transport::{classify_ip, normalize_bare_host, RelayHostClass};

use crate::auth::{BlossomVerb, SignedAuthorization};
use crate::descriptor::{BlobDescriptor, DescriptorError};
use crate::sha256::Sha256Hash;

/// Default cap on a blob-descriptor response body.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 65536;

/// Default overall request deadline (connect, headers, and body).
pub const DEFAULT_REQUEST_DEADLINE: Duration = Duration::from_secs(60);

/// A validated Blossom server base URL: http/https, host present, root
/// path, no query/fragment, no credentials. BUD endpoints live at the
/// domain root (`PUT /upload`), so anything else is a typed refusal
/// rather than a silent rewrite (Destructive-API discipline: this type
/// never normalizes input, it refuses it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlossomServerUrl {
    url: reqwest::Url,
    host: String,
}

/// [`BlossomServerUrl::parse`]'s failure modes. Exhaustive; each variant
/// is pinned by the unit tests below. Checks run host-first so a hostless
/// URL (`mailto:...`) is a `MissingHost` refusal even before its scheme is
/// judged -- both orders fail closed, this one keeps every variant
/// honestly constructible (the url crate guarantees http/https URLs always
/// have a host, so a scheme-first order would make `MissingHost` dead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerUrlError {
    /// Not a parseable URL at all.
    Parse { reason: String },
    /// The URL names no host.
    MissingHost,
    /// A scheme other than `http`/`https`.
    UnsupportedScheme { scheme: String },
    /// URL userinfo would become an ambient `Authorization`-adjacent
    /// credential; refused outright (same rule as the engine's
    /// `CredentialedRelayUrl`).
    Credentialed,
    /// BUD endpoints live at the domain root; a base URL with a path would
    /// silently change every endpoint this client derives.
    NonRootPath { path: String },
    /// A query or fragment would survive into derived endpoint URLs;
    /// refused rather than stripped.
    QueryOrFragment,
}

impl std::fmt::Display for ServerUrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse { reason } => write!(f, "Blossom server URL does not parse: {reason}"),
            Self::MissingHost => f.write_str("Blossom server URL has no host"),
            Self::UnsupportedScheme { scheme } => {
                write!(f, "Blossom server URL scheme {scheme:?} is not http/https")
            }
            Self::Credentialed => f.write_str("Blossom server URL carries credentials"),
            Self::NonRootPath { path } => write!(
                f,
                "Blossom server URL path {path:?} is not the domain root"
            ),
            Self::QueryOrFragment => {
                f.write_str("Blossom server URL carries a query or fragment")
            }
        }
    }
}

impl std::error::Error for ServerUrlError {}

impl BlossomServerUrl {
    /// Validate a Blossom server base URL. See the type doc for the exact
    /// admission rules; nothing is ever normalized away.
    pub fn parse(input: &str) -> Result<Self, ServerUrlError> {
        let url = reqwest::Url::parse(input).map_err(|error| ServerUrlError::Parse {
            reason: error.to_string(),
        })?;
        let host = url
            .host_str()
            .ok_or(ServerUrlError::MissingHost)?
            .to_string();
        let scheme = url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(ServerUrlError::UnsupportedScheme {
                scheme: scheme.to_string(),
            });
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ServerUrlError::Credentialed);
        }
        if !matches!(url.path(), "" | "/") {
            return Err(ServerUrlError::NonRootPath {
                path: url.path().to_string(),
            });
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ServerUrlError::QueryOrFragment);
        }
        Ok(Self { url, host })
    }

    /// The validated base URL text.
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    /// The URL's host component exactly as parsed (IPv6 hosts keep their
    /// brackets, matching `Url::host_str`).
    fn host_str(&self) -> &str {
        &self.host
    }

    /// The BUD-02 upload endpoint at this server's root.
    fn upload_endpoint(&self) -> reqwest::Url {
        let mut url = self.url.clone();
        url.set_path("/upload");
        url
    }
}

/// [`BlossomClient`] construction knobs.
#[derive(Debug, Clone)]
pub struct BlossomClientConfig {
    /// Operator opt-in local-host allowlist, in
    /// `nmp_transport::normalize_bare_host`'s normalized form -- the same
    /// vocabulary the engine's `RelayAdmissionPolicy` uses (issue #519).
    /// Empty (the default) means NO loopback/private/link-local/onion host
    /// or resolved address may be uploaded to.
    pub allowed_local_hosts: BTreeSet<String>,
    /// Cap on the descriptor response body, enforced while streaming.
    pub max_response_bytes: usize,
    /// Overall request deadline (connect, headers, and body).
    pub request_deadline: Duration,
}

impl Default for BlossomClientConfig {
    fn default() -> Self {
        Self {
            allowed_local_hosts: BTreeSet::new(),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            request_deadline: DEFAULT_REQUEST_DEADLINE,
        }
    }
}

/// [`BlossomClient::new`]'s failure: the HTTP stack could not be
/// constructed (system DNS configuration unreadable, or reqwest client
/// construction failed). A struct rather than an enum: both causes are
/// unrecoverable-at-this-layer construction failures a caller handles
/// identically, and neither is reachable on a healthy host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientBuildError {
    pub reason: String,
}

impl std::fmt::Display for ClientBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Blossom HTTP client construction failed: {}", self.reason)
    }
}

impl std::error::Error for ClientBuildError {}

/// [`BlossomClient::upload`]'s exhaustive, separated failure taxonomy
/// (#545). Every variant is constructed by the client and pinned by
/// `tests/upload_contract.rs`; the taxonomy test proves exhaustiveness
/// with a wildcard-free `match`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadError {
    /// The supplied authorization is not an `upload` grant for EXACTLY the
    /// blob bytes being uploaded -- refused before any admission or I/O,
    /// so an authorization can never be replayed across blobs or verbs.
    AuthorizationBlobMismatch {
        expected: Sha256Hash,
        authorized_verb: BlossomVerb,
        authorized_blob: Option<Sha256Hash>,
    },
    /// The server URL names a literal loopback/private/link-local/
    /// unspecified/onion host the operator did not opt in -- refused
    /// before ANY socket I/O (issue #519's discipline).
    LocalHostNotAdmitted { host: String },
    /// Transport failure: connect/DNS/TLS/timeout, or the body stream
    /// died. (A DNS answer that is entirely unadmitted-local also surfaces
    /// here, from the resolver's fail-closed refusal.)
    Network { detail: String },
    /// The server answered with a redirect; redirects are never followed
    /// (an upload must not be silently re-aimed at another authority).
    RedirectRefused { status: u16 },
    /// 401/403: the server refused the authorization itself.
    AuthRejected { status: u16, reason: Option<String> },
    /// Any other non-success, non-5xx status (413, 415, 404, ...): the
    /// server refused this request without indicting the authorization.
    ServerRejected { status: u16, reason: Option<String> },
    /// 5xx: the server failed.
    ServerError { status: u16, reason: Option<String> },
    /// The success body exceeded the configured response cap.
    ResponseTooLarge { limit_bytes: usize },
    /// The success body is not a valid BUD-02 blob descriptor.
    DescriptorInvalid(DescriptorError),
    /// INTEGRITY GATE: the server's descriptor names a different sha256
    /// than the bytes this client just hashed and sent -- fail closed, the
    /// descriptor is never returned.
    Sha256Mismatch {
        expected: Sha256Hash,
        returned: Sha256Hash,
    },
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthorizationBlobMismatch {
                expected,
                authorized_verb,
                authorized_blob,
            } => write!(
                f,
                "authorization ({authorized_verb}, blob {:?}) does not grant uploading blob {}",
                authorized_blob.map(|hash| hash.to_hex()),
                expected.to_hex()
            ),
            Self::LocalHostNotAdmitted { host } => write!(
                f,
                "refusing Blossom upload: host {host:?} is loopback/private/link-local/\
                 unspecified/onion and not operator opted-in"
            ),
            Self::Network { detail } => write!(f, "Blossom upload transport failed: {detail}"),
            Self::RedirectRefused { status } => {
                write!(f, "Blossom upload redirects are not followed (HTTP {status})")
            }
            Self::AuthRejected { status, reason } => write!(
                f,
                "Blossom server rejected the authorization (HTTP {status}, reason {reason:?})"
            ),
            Self::ServerRejected { status, reason } => write!(
                f,
                "Blossom server rejected the upload (HTTP {status}, reason {reason:?})"
            ),
            Self::ServerError { status, reason } => write!(
                f,
                "Blossom server failed (HTTP {status}, reason {reason:?})"
            ),
            Self::ResponseTooLarge { limit_bytes } => {
                write!(f, "Blossom descriptor response exceeds {limit_bytes} bytes")
            }
            Self::DescriptorInvalid(error) => write!(f, "Blossom descriptor invalid: {error}"),
            Self::Sha256Mismatch { expected, returned } => write!(
                f,
                "Blossom server returned sha256 {} for a blob hashing to {} -- refusing the descriptor",
                returned.to_hex(),
                expected.to_hex()
            ),
        }
    }
}

impl std::error::Error for UploadError {}

/// A blob descriptor whose `sha256` was PROVEN equal to the locally
/// computed hash of the exact uploaded bytes. Private field + no public
/// constructor: this type exists only via [`BlossomClient::upload`]'s
/// integrity gate (type-over-convention).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedUpload {
    descriptor: BlobDescriptor,
}

impl VerifiedUpload {
    /// The server's descriptor, integrity-checked against the uploaded
    /// bytes.
    pub fn descriptor(&self) -> &BlobDescriptor {
        &self.descriptor
    }

    /// Consume into the verified descriptor.
    pub fn into_descriptor(self) -> BlobDescriptor {
        self.descriptor
    }
}

/// The async BUD-02 upload client. Construction wires the full engine
/// HTTP discipline (module doc); one client may serve many uploads.
pub struct BlossomClient {
    http: reqwest::Client,
    allowed_local_hosts: Arc<BTreeSet<String>>,
    max_response_bytes: usize,
}

impl BlossomClient {
    /// Build the HTTP client EXACTLY in the engine NIP-11 discipline:
    /// hickory DNS behind a post-resolution local-IP admission filter, no
    /// redirects, no retries, no proxy, no referer, one overall deadline.
    pub fn new(config: BlossomClientConfig) -> Result<Self, ClientBuildError> {
        let allowed_local_hosts = Arc::new(config.allowed_local_hosts);
        let resolver = AdmittedDnsResolver::new(Arc::clone(&allowed_local_hosts))
            .map_err(|reason| ClientBuildError { reason })?;
        let http = reqwest::Client::builder()
            .hickory_dns(true)
            .dns_resolver(Arc::new(resolver))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .referer(false)
            .timeout(config.request_deadline)
            .build()
            .map_err(|error| ClientBuildError {
                reason: error.to_string(),
            })?;
        Ok(Self {
            http,
            allowed_local_hosts,
            max_response_bytes: config.max_response_bytes,
        })
    }

    /// `PUT /upload` of `blob`'s exact bytes, self-verifying end to end:
    ///
    /// 1. refuse an authorization that is not an `upload` grant for
    ///    exactly these bytes (no cross-blob/cross-verb replay);
    /// 2. refuse an unadmitted literal-local host BEFORE any socket I/O;
    /// 3. send `Authorization`, `X-SHA-256`, and (when supplied)
    ///    `Content-Type`; `Content-Length` rides the byte body;
    /// 4. stream the response under the configured byte cap;
    /// 5. map every non-success status into the separated taxonomy;
    /// 6. integrity-gate the descriptor's sha256 against the local hash.
    pub async fn upload(
        &self,
        server: &BlossomServerUrl,
        blob: &[u8],
        content_type: Option<&str>,
        auth: &SignedAuthorization,
    ) -> Result<VerifiedUpload, UploadError> {
        let local_hash = Sha256Hash::of(blob);
        if auth.verb() != BlossomVerb::Upload || auth.blob() != Some(local_hash) {
            return Err(UploadError::AuthorizationBlobMismatch {
                expected: local_hash,
                authorized_verb: auth.verb(),
                authorized_blob: auth.blob(),
            });
        }

        self.reject_unadmitted_local_host(server.host_str())?;

        let mut request = self
            .http
            .put(server.upload_endpoint())
            .header(reqwest::header::AUTHORIZATION, auth.header_value())
            .header("X-SHA-256", local_hash.to_hex());
        if let Some(content_type) = content_type {
            request = request.header(reqwest::header::CONTENT_TYPE, content_type.to_string());
        }
        let mut response = request
            .body(blob.to_vec())
            .send()
            .await
            .map_err(|error| UploadError::Network {
                detail: error.to_string(),
            })?;

        let status = response.status();
        let reason = response
            .headers()
            .get("x-reason")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        match status.as_u16() {
            200 | 201 => {}
            code if status.is_redirection() => {
                return Err(UploadError::RedirectRefused { status: code });
            }
            401 | 403 => {
                return Err(UploadError::AuthRejected {
                    status: status.as_u16(),
                    reason,
                });
            }
            code if status.is_server_error() => {
                return Err(UploadError::ServerError {
                    status: code,
                    reason,
                });
            }
            code => {
                return Err(UploadError::ServerRejected {
                    status: code,
                    reason,
                });
            }
        }

        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| UploadError::Network {
                detail: error.to_string(),
            })?
        {
            if bytes.len().saturating_add(chunk.len()) > self.max_response_bytes {
                return Err(UploadError::ResponseTooLarge {
                    limit_bytes: self.max_response_bytes,
                });
            }
            bytes.extend_from_slice(&chunk);
        }

        let descriptor =
            BlobDescriptor::parse_json(&bytes).map_err(UploadError::DescriptorInvalid)?;
        if descriptor.sha256 != local_hash {
            return Err(UploadError::Sha256Mismatch {
                expected: local_hash,
                returned: descriptor.sha256,
            });
        }
        Ok(VerifiedUpload { descriptor })
    }

    /// Refuse a literal loopback/private/link-local/unspecified/onion HOST
    /// that the operator did not explicitly opt in -- BEFORE any request
    /// is built (issue #519's discipline; the resolver below cannot see an
    /// IP-literal host because a literal never reaches DNS). Mirrors the
    /// hostname-level rules of `nmp-transport::classify_relay_host`
    /// (`classify_relay_host` itself takes a `RelayUrl`, which does not
    /// fit an http URL).
    fn reject_unadmitted_local_host(&self, host: &str) -> Result<(), UploadError> {
        let bare = host
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .unwrap_or(host);
        if literal_host_class(bare) == RelayHostClass::Local
            && !self.allowed_local_hosts.contains(&normalize_bare_host(bare))
        {
            return Err(UploadError::LocalHostNotAdmitted {
                host: host.to_string(),
            });
        }
        Ok(())
    }
}

/// Classify a bare host string (brackets already stripped) by the SAME
/// rules `nmp-transport::classify_relay_host` applies to a relay URL's
/// host: IP literals through `classify_ip`, and the `localhost`/
/// `*.localhost`/`*.onion` domain rules for names.
fn literal_host_class(bare_host: &str) -> RelayHostClass {
    match bare_host.parse::<IpAddr>() {
        Ok(ip) => classify_ip(ip),
        Err(_) => {
            let normalized = normalize_bare_host(bare_host);
            if normalized == "localhost"
                || normalized.ends_with(".localhost")
                || normalized.ends_with(".onion")
            {
                RelayHostClass::Local
            } else {
                RelayHostClass::Public
            }
        }
    }
}

/// The post-DNS half of admission (issue #519's discipline, reimplemented
/// engine-free): resolve through hickory, then drop every answer that
/// classifies `Local` unless the queried name itself was operator opted
/// in; if EVERY answer is dropped the whole lookup fails closed with the
/// real reason.
struct AdmittedDnsResolver {
    resolver: hickory_resolver::TokioResolver,
    allowed_local_hosts: Arc<BTreeSet<String>>,
}

impl AdmittedDnsResolver {
    fn new(allowed_local_hosts: Arc<BTreeSet<String>>) -> Result<Self, String> {
        let mut builder = hickory_resolver::TokioResolver::builder_tokio()
            .map_err(|error| format!("could not read the system DNS configuration: {error}"))?;
        builder.options_mut().ip_strategy =
            hickory_resolver::config::LookupIpStrategy::Ipv4AndIpv6;
        Ok(Self {
            resolver: builder.build(),
            allowed_local_hosts,
        })
    }
}

impl reqwest::dns::Resolve for AdmittedDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let resolver = self.resolver.clone();
        let allowed_local_hosts = Arc::clone(&self.allowed_local_hosts);
        let query_name = name.as_str().to_string();
        Box::pin(async move {
            let lookup = resolver.lookup_ip(query_name.clone()).await?;
            let host_opted_in = allowed_local_hosts.contains(&normalize_bare_host(&query_name));
            let mut admitted = Vec::new();
            for address in lookup {
                if classify_ip(address) == RelayHostClass::Local && !host_opted_in {
                    continue;
                }
                admitted.push(std::net::SocketAddr::new(address, 0));
            }
            if admitted.is_empty() {
                let message = format!(
                    "refusing to resolve {query_name}: every resolved address is \
                     loopback/private/link-local/unspecified and the host is not operator \
                     opted-in"
                );
                return Err(Box::<dyn std::error::Error + Send + Sync>::from(message));
            }
            let addrs: reqwest::dns::Addrs = Box::new(admitted.into_iter());
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invariant (#545): a root-path http/https server URL is admitted
    /// (with or without an explicit trailing slash, with a port).
    #[test]
    fn root_path_http_and_https_urls_are_admitted() {
        assert!(BlossomServerUrl::parse("https://cdn.example.com").is_ok());
        assert!(BlossomServerUrl::parse("https://cdn.example.com/").is_ok());
        assert!(BlossomServerUrl::parse("http://127.0.0.1:3000").is_ok());
    }

    /// Invariant (#545): every [`ServerUrlError`] variant is a real,
    /// constructible refusal -- scheme, path, credentials, query/fragment,
    /// hostless, and unparseable inputs each hit their own variant.
    #[test]
    fn each_server_url_refusal_is_its_own_typed_variant() {
        assert!(matches!(
            BlossomServerUrl::parse("not a url"),
            Err(ServerUrlError::Parse { .. })
        ));
        assert_eq!(
            BlossomServerUrl::parse("mailto:user@example.com"),
            Err(ServerUrlError::MissingHost)
        );
        assert_eq!(
            BlossomServerUrl::parse("ftp://cdn.example.com"),
            Err(ServerUrlError::UnsupportedScheme {
                scheme: "ftp".to_string()
            })
        );
        assert_eq!(
            BlossomServerUrl::parse("https://user:secret@cdn.example.com"),
            Err(ServerUrlError::Credentialed)
        );
        assert_eq!(
            BlossomServerUrl::parse("https://cdn.example.com/media"),
            Err(ServerUrlError::NonRootPath {
                path: "/media".to_string()
            })
        );
        assert_eq!(
            BlossomServerUrl::parse("https://cdn.example.com/?a=1"),
            Err(ServerUrlError::QueryOrFragment)
        );
        assert_eq!(
            BlossomServerUrl::parse("https://cdn.example.com/#frag"),
            Err(ServerUrlError::QueryOrFragment)
        );
    }

    /// Invariant (#545): the literal-host classifier mirrors
    /// `nmp-transport`'s admission rules exactly -- IP literals via
    /// `classify_ip`, `localhost`/`.localhost`/`.onion` names local,
    /// ordinary public names public.
    #[test]
    fn literal_host_classification_mirrors_transport_admission_rules() {
        assert_eq!(literal_host_class("127.0.0.1"), RelayHostClass::Local);
        assert_eq!(literal_host_class("10.0.0.1"), RelayHostClass::Local);
        assert_eq!(literal_host_class("::1"), RelayHostClass::Local);
        assert_eq!(literal_host_class("localhost"), RelayHostClass::Local);
        assert_eq!(literal_host_class("LOCALHOST"), RelayHostClass::Local);
        assert_eq!(literal_host_class("foo.localhost"), RelayHostClass::Local);
        assert_eq!(
            literal_host_class("expyuzz4wqqyqhjn.onion"),
            RelayHostClass::Local
        );
        assert_eq!(
            literal_host_class("cdn.example.com"),
            RelayHostClass::Public
        );
        assert_eq!(literal_host_class("8.8.8.8"), RelayHostClass::Public);
    }
}
