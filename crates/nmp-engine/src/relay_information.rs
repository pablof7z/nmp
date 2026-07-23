//! Engine-owned, one-shot NIP-11 acquisition.
//!
//! NIP-11 is HTTP state, not a reactive stream. This service gives callers
//! an explicit one-shot read while sharing a bounded, in-memory cache and a
//! per-relay single flight. The last good document is retained separately
//! from the last acquisition error, so a transient failure never destroys
//! useful presentation or capability evidence.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use crossbeam_channel::{bounded, Receiver, Sender};
use futures_channel::oneshot;
use nmp_transport::{
    classify_ip, classify_relay_host, normalize_bare_host, relay_host_key, RelayHostClass,
};
use nostr::{types::url::Host, RelayUrl};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

const DEFAULT_FRESH_FOR: Duration = Duration::from_secs(60 * 60);
// Engine teardown has a public <5s lifecycle falsifier. This is an overall
// request deadline (headers and body), not a per-read timeout, so a peer that
// accepts a connection and then stops responding cannot hold shutdown past
// that contract.
const FETCH_DEADLINE: Duration = Duration::from_secs(3);
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
const CACHE_CAPACITY: usize = 256;
/// One engine may have at most this many distinct-relay HTTP/DNS/body
/// acquisitions live at once. Additional callers remain in their own futures
/// awaiting a semaphore permit; they are never retained in a service queue and
/// never receive a public saturation error.
const MAX_ACTIVE_FETCHES: usize = 8;

/// Whether a one-shot read may use a still-fresh cached result or must
/// revalidate/refetch it. Concurrent reads of either kind still share one
/// in-flight request per canonical relay URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayInformationCachePolicy {
    UseCache,
    Refresh,
}

/// Freshness of the returned last-good document at the instant it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayInformationFreshness {
    Fresh,
    Stale,
}

/// A typed acquisition failure. HTTP and parse failures are deliberately
/// values; they are never represented as an empty relay document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayInformationError {
    ServiceClosed,
    /// Relay URL credentials are rejected before an HTTP request is
    /// constructed; reqwest otherwise converts them into a Basic
    /// `Authorization` header.
    CredentialedRelayUrl,
    Http {
        reason: String,
    },
    ResponseTooLarge {
        limit_bytes: u64,
    },
    InvalidDocument {
        reason: String,
    },
}

impl std::fmt::Display for RelayInformationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ServiceClosed => f.write_str("NIP-11 acquisition service is closed"),
            Self::CredentialedRelayUrl => {
                f.write_str("NIP-11 acquisition refuses relay URL userinfo")
            }
            Self::Http { reason } => write!(f, "NIP-11 HTTP request failed: {reason}"),
            Self::ResponseTooLarge { limit_bytes } => {
                write!(f, "NIP-11 response exceeds {limit_bytes} bytes")
            }
            Self::InvalidDocument { reason } => write!(f, "invalid NIP-11 document: {reason}"),
        }
    }
}

impl std::error::Error for RelayInformationError {}

/// Presentation and capability fields NMP understands today. `raw_json` on
/// [`RelayInformationSnapshot`] remains the forward-compatible authority;
/// unknown fields are not discarded just because this typed projection has
/// not learned them yet.
#[derive(Debug, Clone, PartialEq)]
pub struct RelayInformationDocument {
    pub name: Option<String>,
    pub description: Option<String>,
    pub banner: Option<String>,
    pub icon: Option<String>,
    pub pubkey: Option<String>,
    pub self_pubkey: Option<String>,
    pub contact: Option<String>,
    /// `None` means the relay did not advertise a list. `Some(empty)` is an
    /// explicit advertisement that no NIPs are supported.
    pub supported_nips: Option<Vec<u16>>,
    pub software: Option<String>,
    pub version: Option<String>,
    pub terms_of_service: Option<String>,
    /// Advisory limits claimed by the relay. These are never runtime proof
    /// and a planner may only consume them when it can remain exact or
    /// surface an explicit shortfall.
    pub limitation: RelayInformationLimitations,
    /// Exact JSON fragments for structured fields whose schema evolves
    /// independently (`limitation`, `fees`, ...).
    pub structured: BTreeMap<String, String>,
}

/// The current well-known NIP-11 limitation fields. Every field is optional
/// because omission is unknown, never an implicit zero/false claim. The
/// enclosing document's `structured["limitation"]` retains the exact object,
/// including fields this projection does not yet understand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayInformationLimitations {
    pub max_message_length: Option<u64>,
    pub max_subscriptions: Option<u64>,
    pub max_filters: Option<u64>,
    pub max_limit: Option<u64>,
    pub max_subid_length: Option<u64>,
    pub max_event_tags: Option<u64>,
    pub max_content_length: Option<u64>,
    pub min_pow_difficulty: Option<u64>,
    pub auth_required: Option<bool>,
    pub payment_required: Option<bool>,
    pub created_at_lower_limit: Option<u64>,
    pub created_at_upper_limit: Option<u64>,
}

/// One last-good NIP-11 document plus acquisition metadata.
///
/// Cloning this mechanism value is deliberately shallow. The exact raw body,
/// parsed document (including structured maps), and revision live in one
/// immutable payload shared by the cache, a refreshing worker, every waiter,
/// and the runtime's capability projection. Metadata-only transitions such as
/// 304 revalidation and stale-on-error create another immutable version that
/// cites the same payload.
#[derive(Debug, Clone, PartialEq)]
pub struct RelayInformationSnapshot {
    inner: Arc<RelayInformationSnapshotVersion>,
}

#[derive(Debug, PartialEq)]
struct RelayInformationSnapshotVersion {
    payload: Arc<RelayInformationSnapshotPayload>,
    fetched_at: u64,
    fresh_until: u64,
    freshness: RelayInformationFreshness,
    etag: Option<String>,
    last_modified: Option<String>,
    cache_control: Option<String>,
    expires: Option<String>,
    last_error: Option<RelayInformationError>,
}

#[derive(Debug, PartialEq)]
struct RelayInformationSnapshotPayload {
    relay: RelayUrl,
    document: RelayInformationDocument,
    raw_json: String,
    /// Stable BLAKE3 identity of the exact received JSON representation.
    /// Capability facts cite this revision rather than an unscoped boolean.
    document_revision: String,
}

impl RelayInformationSnapshot {
    #[allow(clippy::too_many_arguments)]
    fn new(
        relay: RelayUrl,
        document: RelayInformationDocument,
        raw_json: String,
        document_revision: String,
        fetched_at: u64,
        fresh_until: u64,
        freshness: RelayInformationFreshness,
        etag: Option<String>,
        last_modified: Option<String>,
        cache_control: Option<String>,
        expires: Option<String>,
        last_error: Option<RelayInformationError>,
    ) -> Self {
        Self {
            inner: Arc::new(RelayInformationSnapshotVersion {
                payload: Arc::new(RelayInformationSnapshotPayload {
                    relay,
                    document,
                    raw_json,
                    document_revision,
                }),
                fetched_at,
                fresh_until,
                freshness,
                etag,
                last_modified,
                cache_control,
                expires,
                last_error,
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn with_metadata(
        &self,
        fetched_at: u64,
        fresh_until: u64,
        freshness: RelayInformationFreshness,
        etag: Option<String>,
        last_modified: Option<String>,
        cache_control: Option<String>,
        expires: Option<String>,
        last_error: Option<RelayInformationError>,
    ) -> Self {
        Self {
            inner: Arc::new(RelayInformationSnapshotVersion {
                payload: Arc::clone(&self.inner.payload),
                fetched_at,
                fresh_until,
                freshness,
                etag,
                last_modified,
                cache_control,
                expires,
                last_error,
            }),
        }
    }

    fn with_read_state(
        &self,
        freshness: RelayInformationFreshness,
        last_error: Option<RelayInformationError>,
    ) -> Self {
        self.with_metadata(
            self.fetched_at(),
            self.fresh_until(),
            freshness,
            self.etag().map(str::to_owned),
            self.last_modified().map(str::to_owned),
            self.cache_control().map(str::to_owned),
            self.expires().map(str::to_owned),
            last_error,
        )
    }

    pub fn relay(&self) -> &RelayUrl {
        &self.inner.payload.relay
    }

    pub fn document(&self) -> &RelayInformationDocument {
        &self.inner.payload.document
    }

    pub fn raw_json(&self) -> &str {
        &self.inner.payload.raw_json
    }

    pub fn document_revision(&self) -> &str {
        &self.inner.payload.document_revision
    }

    pub fn fetched_at(&self) -> u64 {
        self.inner.fetched_at
    }

    pub fn fresh_until(&self) -> u64 {
        self.inner.fresh_until
    }

    pub fn freshness(&self) -> RelayInformationFreshness {
        self.inner.freshness
    }

    pub fn etag(&self) -> Option<&str> {
        self.inner.etag.as_deref()
    }

    pub fn last_modified(&self) -> Option<&str> {
        self.inner.last_modified.as_deref()
    }

    pub fn cache_control(&self) -> Option<&str> {
        self.inner.cache_control.as_deref()
    }

    pub fn expires(&self) -> Option<&str> {
        self.inner.expires.as_deref()
    }

    pub fn last_error(&self) -> Option<&RelayInformationError> {
        self.inner.last_error.as_ref()
    }

    /// Advertisement only. This never creates a behavioral capability token.
    pub fn advertises_nip(&self, nip: u16) -> Option<bool> {
        self.document()
            .supported_nips
            .as_ref()
            .map(|nips| nips.contains(&nip))
    }

    pub(crate) fn capability_evidence(&self) -> RelayInformationCapabilityEvidence {
        RelayInformationCapabilityEvidence {
            supported_nips: self.document().supported_nips.clone(),
            document_revision: self.document_revision().to_owned(),
            fresh_until: self.fresh_until(),
            last_error: self.last_error().cloned(),
        }
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    fn payload_identity_value(&self) -> usize {
        Arc::as_ptr(&self.inner.payload) as usize
    }

    #[cfg(test)]
    fn payload_identity(&self) -> usize {
        self.payload_identity_value()
    }
}

/// The provenance-bearing subset of a NIP-11 snapshot used by engine
/// capability decisions and diagnostics. It deliberately excludes runtime
/// connection/AUTH state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayInformationCapabilityEvidence {
    pub supported_nips: Option<Vec<u16>>,
    pub document_revision: String,
    /// Absolute Unix-seconds deadline. Diagnostics derives freshness from
    /// the engine clock instead of retaining a read-time label forever.
    pub fresh_until: u64,
    pub last_error: Option<RelayInformationError>,
}

#[derive(Clone)]
pub struct RelayInformationService {
    shared: Arc<Shared>,
    runtime: tokio::runtime::Handle,
    fetcher: Arc<dyn Fetcher>,
}

struct Shared {
    state: Mutex<State>,
    access_clock: AtomicU64,
    next_flight: AtomicU64,
    cache_capacity: usize,
    fetch_slots: Arc<Semaphore>,
}

/// Mechanism-only retention evidence used to falsify cache/flight ownership.
/// Caller-owned values materialized by the supported `nmp` facade are outside
/// this census by design.
#[doc(hidden)]
#[cfg(any(test, feature = "test-instrumentation"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayInformationRetentionCensus {
    pub cached_entries: usize,
    pub cached_payloads: usize,
    pub cached_raw_body_bytes: usize,
    pub active_flights: usize,
    pub subscribed_callers: usize,
    pub max_active_flights: usize,
}

struct State {
    closed: bool,
    entries: HashMap<RelayUrl, Entry>,
}

#[derive(Default)]
struct Entry {
    cached: Option<Cached>,
    flight: Option<Flight>,
    last_access: u64,
}

struct Flight {
    generation: u64,
    completion: watch::Sender<Option<Result<RelayInformationSnapshot, RelayInformationError>>>,
    cancellation: Arc<CancelSignal>,
    /// Dropping the exact flight releases its one physical HTTP/DNS/body slot.
    _permit: OwnedSemaphorePermit,
}

struct CancelSignal {
    sender: Mutex<Option<oneshot::Sender<()>>>,
}

impl CancelSignal {
    fn cancel(&self) {
        if let Some(sender) = self
            .sender
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            let _ = sender.send(());
        }
    }
}

struct FetchCancellation {
    receiver: oneshot::Receiver<()>,
}

#[derive(Clone)]
struct Cached {
    snapshot: RelayInformationSnapshot,
    fresh_until: u64,
}

#[derive(Debug)]
struct FetchResult {
    raw_json: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
    cache_control: Option<String>,
    expires: Option<String>,
    fresh_for: Option<Duration>,
}

trait Fetcher: Send + Sync + 'static {
    /// Run one NIP-11 acquisition as an async task on the engine runtime.
    /// The returned future selects on `cancellation` so engine teardown
    /// interrupts DNS, connect, headers, and body (the production HTTP
    /// implementation); deterministic test fetchers ignore it and are
    /// released by their own harness.
    fn fetch_cancellable_async<'a>(
        &'a self,
        relay: RelayUrl,
        validators: Option<(String, String)>,
        cancellation: FetchCancellation,
    ) -> Pin<Box<dyn Future<Output = Result<FetchResult, RelayInformationError>> + Send + 'a>>;
}

struct HttpFetcher {
    resolver_config: Option<hickory_resolver::config::ResolverConfig>,
    resolver_strategy: hickory_resolver::config::LookupIpStrategy,
    /// Operator opt-in local-host allowlist (issue #519), in
    /// [`nmp_transport::relay_host_key`]'s normalized form — the SAME set
    /// `nmp-engine`'s `RelayAdmissionPolicy` enforces at discovery-time
    /// admission. Empty (the default from [`Self::new`]) means NO host may
    /// fetch NIP-11 over a loopback/private/link-local/unspecified/onion
    /// host or resolved address; production wiring passes the engine's real
    /// allowlist via [`Self::new_with_admission`].
    allowed_local_hosts: Arc<BTreeSet<String>>,
}

/// An HTTP URL whose authority has been proven not to contain userinfo.
/// Keeping this distinct from `String` makes the no-Authorization invariant
/// a prerequisite of `fetch_http`, not a request-builder convention.
struct UncredentialedHttpUrl(reqwest::Url);

impl HttpFetcher {
    fn new() -> Self {
        Self {
            resolver_config: None,
            resolver_strategy: hickory_resolver::config::LookupIpStrategy::Ipv4AndIpv6,
            allowed_local_hosts: Arc::new(BTreeSet::new()),
        }
    }

    /// Production constructor (issue #519): identical to [`Self::new`] but
    /// carries the engine's real opt-in local-host allowlist so an
    /// operator-configured local relay's NIP-11 document is still reachable
    /// after the resolved-IP admission check below refuses everything else.
    fn new_with_admission(allowed_local_hosts: Arc<BTreeSet<String>>) -> Self {
        Self {
            allowed_local_hosts,
            ..Self::new()
        }
    }

    #[cfg(test)]
    fn with_resolver_config(config: hickory_resolver::config::ResolverConfig) -> Self {
        Self {
            resolver_config: Some(config),
            resolver_strategy: hickory_resolver::config::LookupIpStrategy::Ipv4Only,
            allowed_local_hosts: Arc::new(BTreeSet::new()),
        }
    }

    #[cfg(test)]
    fn with_resolver_config_and_admission(
        config: hickory_resolver::config::ResolverConfig,
        allowed_local_hosts: Arc<BTreeSet<String>>,
    ) -> Self {
        Self {
            allowed_local_hosts,
            ..Self::with_resolver_config(config)
        }
    }
}

impl HttpFetcher {
    /// Blocking one-shot fetch for the several synchronous `#[test]` callers.
    /// It builds a private current-thread runtime and drives the same async
    /// acquisition the engine runtime would; production never uses this path
    /// (the engine spawns [`Fetcher::fetch_cancellable_async`] directly).
    #[cfg(test)]
    fn fetch(
        &self,
        relay: &RelayUrl,
        validators: Option<(&str, &str)>,
    ) -> Result<FetchResult, RelayInformationError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| RelayInformationError::Http {
                reason: format!("HTTP runtime: {error}"),
            })?;
        let (_cancel, receiver) = oneshot::channel();
        runtime.block_on(self.fetch_cancellable_async(
            relay.clone(),
            validators.map(|(etag, last_modified)| (etag.to_string(), last_modified.to_string())),
            FetchCancellation { receiver },
        ))
    }
}

impl Fetcher for HttpFetcher {
    fn fetch_cancellable_async<'a>(
        &'a self,
        relay: RelayUrl,
        validators: Option<(String, String)>,
        cancellation: FetchCancellation,
    ) -> Pin<Box<dyn Future<Output = Result<FetchResult, RelayInformationError>> + Send + 'a>> {
        Box::pin(async move {
            // Issue #519 (HIGH): refuse a literal loopback/private/link-local/
            // unspecified/onion HOST before a request is even built. This is
            // the ONLY defense for an IP-literal relay URL (`ws://127.0.0.1`)
            // — a literal address never reaches the DNS resolver below, so the
            // resolver's own filtering can't see it. Matches the SAME
            // classification `nmp-transport::classify_relay_host` applies at
            // discovery-time admission; an operator-opted-in host still passes.
            reject_unadmitted_local_host(&relay, &self.allowed_local_hosts)?;
            let url = relay_http_url(&relay)?;
            let allowed_local_hosts = Arc::clone(&self.allowed_local_hosts);
            let request = fetch_http(
                url,
                validators,
                self.resolver_config.clone(),
                self.resolver_strategy,
                allowed_local_hosts,
            );
            let mut request = Box::pin(request);
            let mut cancelled = Box::pin(cancellation.receiver);
            let selected = std::future::poll_fn(move |cx| {
                if cancelled.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Err(RelayInformationError::ServiceClosed));
                }
                request.as_mut().poll(cx)
            });
            tokio::time::timeout(FETCH_DEADLINE, selected)
                .await
                .map_err(|_| RelayInformationError::Http {
                    reason: format!(
                        "overall NIP-11 request deadline exceeded after {}s",
                        FETCH_DEADLINE.as_secs()
                    ),
                })?
        })
    }
}

/// Refuse `relay` outright if its URL names a literal loopback/private/
/// link-local/unspecified/onion HOST that the operator did not explicitly
/// opt in (issue #519). Pure and DNS-free — the same classification
/// `nmp-transport::classify_relay_host` applies at discovery-time admission,
/// checked again here because `Handle::relay_information` is a public API
/// any caller can invoke for ANY relay URL, admitted into the routable
/// directory or not.
fn reject_unadmitted_local_host(
    relay: &RelayUrl,
    allowed_local_hosts: &BTreeSet<String>,
) -> Result<(), RelayInformationError> {
    if classify_relay_host(relay) == RelayHostClass::Local
        && !relay_host_key(relay).is_some_and(|host| allowed_local_hosts.contains(&host))
    {
        return Err(RelayInformationError::Http {
            reason: "refusing NIP-11 fetch: relay host is loopback/private/link-local/\
                     unspecified/onion and not operator opted-in"
                .to_string(),
        });
    }
    Ok(())
}

async fn fetch_http(
    url: UncredentialedHttpUrl,
    validators: Option<(String, String)>,
    resolver_config: Option<hickory_resolver::config::ResolverConfig>,
    resolver_strategy: hickory_resolver::config::LookupIpStrategy,
    allowed_local_hosts: Arc<BTreeSet<String>>,
) -> Result<FetchResult, RelayInformationError> {
    // The client is deliberately born and dropped inside this flight's
    // current-thread runtime. Hickory therefore cannot retain runtime-bound
    // DNS work, and no client clone can outlive the owned executor task. An
    // IP-literal URL bypasses DNS in reqwest, so do not synchronously read the
    // host's resolver configuration for work reqwest will never request. The
    // literal address was already admitted by `reject_unadmitted_local_host`.
    let mut client_builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .referer(false)
        .timeout(FETCH_DEADLINE);
    if matches!(url.0.host(), Some(Host::Domain(_))) {
        let resolver =
            HickoryReqwestResolver::new(resolver_config, resolver_strategy, allowed_local_hosts)?;
        client_builder = client_builder.dns_resolver(Arc::new(resolver));
    }
    let client = client_builder
        .build()
        .map_err(|error| RelayInformationError::Http {
            reason: format!("HTTP client construction failed: {error}"),
        })?;
    // `url` can only be built by `relay_http_url`, which rejects URL
    // credentials before this request builder exists; an empty userinfo marker
    // has already normalized to a credential-free typed URL. Proxies,
    // redirects, referrers, and retries are disabled above, so no other
    // URL-derived authentication or authority hop exists. Conditional headers
    // below are server-provided validators and still pass HeaderValue checks.
    let mut request = client
        .get(url.0)
        .header(reqwest::header::ACCEPT, "application/nostr+json");
    if let Some((etag, last_modified)) = validators {
        if !etag.is_empty() {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if !last_modified.is_empty() {
            request = request.header(reqwest::header::IF_MODIFIED_SINCE, last_modified);
        }
    }
    let mut response = request
        .send()
        .await
        .map_err(|error| RelayInformationError::Http {
            reason: error.to_string(),
        })?;
    let status = response.status();
    if status.is_redirection() && status != reqwest::StatusCode::NOT_MODIFIED {
        return Err(RelayInformationError::Http {
            reason: "NIP-11 redirects are not followed".to_string(),
        });
    }
    if status != reqwest::StatusCode::NOT_MODIFIED && !status.is_success() {
        return Err(RelayInformationError::Http {
            reason: format!("NIP-11 HTTP status {status}"),
        });
    }
    let header = |name: reqwest::header::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let cache_control = header(reqwest::header::CACHE_CONTROL);
    let expires = header(reqwest::header::EXPIRES);
    let etag = header(reqwest::header::ETAG);
    let last_modified = header(reqwest::header::LAST_MODIFIED);
    let fresh_for = fresh_for_headers(
        cache_control.as_deref(),
        expires.as_deref(),
        SystemTime::now(),
    );
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(FetchResult {
            raw_json: None,
            etag,
            last_modified,
            cache_control,
            expires,
            fresh_for,
        });
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| RelayInformationError::Http {
            reason: error.to_string(),
        })?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES as usize {
            return Err(RelayInformationError::ResponseTooLarge {
                limit_bytes: MAX_RESPONSE_BYTES,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    let raw_json =
        String::from_utf8(bytes).map_err(|error| RelayInformationError::InvalidDocument {
            reason: error.to_string(),
        })?;
    Ok(FetchResult {
        raw_json: Some(raw_json),
        etag,
        last_modified,
        cache_control,
        expires,
        fresh_for,
    })
}

#[derive(Clone)]
struct HickoryReqwestResolver {
    resolver: hickory_resolver::TokioResolver,
    /// See [`HttpFetcher::allowed_local_hosts`] — the same set, threaded
    /// down so a resolved answer for an opted-in host is still admitted
    /// (issue #519).
    allowed_local_hosts: Arc<BTreeSet<String>>,
}

impl HickoryReqwestResolver {
    fn new(
        config: Option<hickory_resolver::config::ResolverConfig>,
        strategy: hickory_resolver::config::LookupIpStrategy,
        allowed_local_hosts: Arc<BTreeSet<String>>,
    ) -> Result<Self, RelayInformationError> {
        let mut builder = match config {
            Some(config) => hickory_resolver::TokioResolver::builder_with_config(
                config,
                hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
            ),
            None => hickory_resolver::TokioResolver::builder_tokio().map_err(|error| {
                RelayInformationError::Http {
                    reason: format!("could not read the system DNS configuration: {error}"),
                }
            })?,
        };
        builder.options_mut().ip_strategy = strategy;
        let resolver = builder
            .build()
            .map_err(|error| RelayInformationError::Http {
                reason: format!("could not construct the DNS resolver: {error}"),
            })?;
        Ok(Self {
            resolver,
            allowed_local_hosts,
        })
    }
}

impl reqwest::dns::Resolve for HickoryReqwestResolver {
    /// Resolve `name` and refuse (issue #519, HIGH) any answer that
    /// classifies `Local` (loopback/RFC-1918/link-local/unspecified/IPv4-
    /// mapped-private) unless `name` itself was operator opted in. If EVERY
    /// resolved address is `Local` and not opted in, the whole lookup fails
    /// closed — an empty `Addrs` would otherwise surface as a confusing
    /// "connect to nothing" error further down reqwest's stack, whereas an
    /// explicit `Err` here reports the real reason immediately. A host with
    /// a MIX of local and public answers keeps only the public ones (the
    /// common, benign case of a resolver also handing back an IPv6
    /// link-local scope address alongside a real public one).
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let resolver = self.resolver.clone();
        let allowed_local_hosts = Arc::clone(&self.allowed_local_hosts);
        let query_name = name.as_str().to_string();
        Box::pin(async move {
            let lookup = resolver.lookup_ip(query_name.clone()).await?;
            let host_opted_in = allowed_local_hosts.contains(&normalize_bare_host(&query_name));
            let mut admitted = Vec::new();
            for address in lookup.iter() {
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

fn fresh_for_headers(
    cache_control: Option<&str>,
    expires: Option<&str>,
    now: SystemTime,
) -> Option<Duration> {
    if let Some(cache_control) = cache_control {
        let mut max_age = None;
        for directive in cache_control.split(',').map(str::trim) {
            if directive.eq_ignore_ascii_case("no-cache")
                || directive.eq_ignore_ascii_case("no-store")
            {
                return Some(Duration::ZERO);
            }
            if let Some((name, value)) = directive.split_once('=') {
                if name.trim().eq_ignore_ascii_case("max-age") {
                    max_age = value
                        .trim()
                        .trim_matches('"')
                        .parse::<u64>()
                        .ok()
                        .map(Duration::from_secs);
                }
            }
        }
        if max_age.is_some() {
            return max_age;
        }
    }

    let expires = httpdate::parse_http_date(expires?).ok()?;
    Some(expires.duration_since(now).unwrap_or_default())
}

fn relay_http_url(relay: &RelayUrl) -> Result<UncredentialedHttpUrl, RelayInformationError> {
    let source: &reqwest::Url = relay.into();
    let serialized = source.as_str();
    let authority_has_userinfo = serialized
        .split_once("://")
        .map(|(_, rest)| {
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            rest[..end].contains('@')
        })
        .unwrap_or(false);
    if authority_has_userinfo || !source.username().is_empty() || source.password().is_some() {
        return Err(RelayInformationError::CredentialedRelayUrl);
    }

    let mut http = source.clone();
    let scheme = if source.scheme() == "wss" {
        "https"
    } else {
        "http"
    };
    http.set_scheme(scheme)
        .map_err(|_| RelayInformationError::Http {
            reason: "could not translate relay URL to HTTP".to_string(),
        })?;
    debug_assert!(http.username().is_empty());
    debug_assert!(http.password().is_none());
    Ok(UncredentialedHttpUrl(http))
}

impl RelayInformationService {
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        Self::with_runtime_and_limits(runtime, Arc::new(HttpFetcher::new()), CACHE_CAPACITY)
    }

    /// Production constructor (issue #519): identical to [`Self::new`] but
    /// carries the engine's real `RelayAdmissionPolicy` opt-in local-host
    /// allowlist through to the NIP-11 fetcher's resolved-IP admission
    /// check, so an operator-configured local relay's document is still
    /// reachable — see `EngineThread::spawn_with_native_task_limit`, the one
    /// production call site.
    pub(crate) fn new_with_admission(
        runtime: tokio::runtime::Handle,
        allowed_local_hosts: Arc<BTreeSet<String>>,
    ) -> Self {
        Self::with_runtime_and_limits(
            runtime,
            Arc::new(HttpFetcher::new_with_admission(allowed_local_hosts)),
            CACHE_CAPACITY,
        )
    }

    #[cfg(test)]
    fn try_with_fetcher(fetcher: Arc<dyn Fetcher>) -> std::io::Result<Self> {
        Self::with_fetcher_and_capacity(fetcher, CACHE_CAPACITY)
    }

    #[cfg(test)]
    fn with_fetcher_and_capacity(
        fetcher: Arc<dyn Fetcher>,
        cache_capacity: usize,
    ) -> std::io::Result<Self> {
        // The engine owns the runtime in production; a test that only needs a
        // fetcher builds and intentionally leaks a small multi-thread runtime
        // so its handle stays valid for the whole test (a bare `Handle` does
        // not keep the runtime alive). Four workers keep several concurrently
        // gated flights from deadlocking their blocking test harness.
        let runtime = Box::leak(Box::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()?,
        ));
        Ok(Self::with_runtime_and_limits(
            runtime.handle().clone(),
            fetcher,
            cache_capacity,
        ))
    }

    fn with_runtime_and_limits(
        runtime: tokio::runtime::Handle,
        fetcher: Arc<dyn Fetcher>,
        cache_capacity: usize,
    ) -> Self {
        assert!(cache_capacity > 0, "NIP-11 cache capacity must be non-zero");
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                closed: false,
                entries: HashMap::new(),
            }),
            access_clock: AtomicU64::new(0),
            next_flight: AtomicU64::new(1),
            cache_capacity,
            fetch_slots: Arc::new(Semaphore::new(MAX_ACTIVE_FETCHES)),
        });
        Self {
            shared,
            runtime,
            fetcher,
        }
    }

    /// Read relay information once. Fresh cached values return immediately.
    /// A cold distinct-relay miss waits on the caller thread for bounded async
    /// admission; it is never queued in the service or refused as saturation.
    pub fn get(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
    ) -> Result<RelayInformationSnapshot, RelayInformationError> {
        block_on_caller(self.get_async(relay, policy))
    }

    /// Read relay information without blocking the caller. At most
    /// `MAX_ACTIVE_FETCHES` distinct HTTP/DNS/body tasks are live; excess
    /// distinct-relay callers suspend in their own futures awaiting admission.
    /// Same-relay callers subscribe to one shared completion and therefore add
    /// neither another fetch task nor a service-owned waiter record.
    pub async fn get_async(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
    ) -> Result<RelayInformationSnapshot, RelayInformationError> {
        let mut permit = None;
        loop {
            match self.register(relay.clone(), policy, permit.take())? {
                Registration::Ready(result) => return result,
                Registration::Flight(wait) => return wait.wait().await,
                Registration::NeedsAdmission => {
                    permit = Some(
                        Arc::clone(&self.shared.fetch_slots)
                            .acquire_owned()
                            .await
                            .map_err(|_| RelayInformationError::ServiceClosed)?,
                    );
                }
            }
        }
    }

    pub(crate) fn request_callback(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
        callback: impl FnOnce(Result<RelayInformationSnapshot, RelayInformationError>) + Send + 'static,
    ) -> Result<(), RelayInformationError> {
        if self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .closed
        {
            return Err(RelayInformationError::ServiceClosed);
        }
        let service = self.clone();
        self.runtime.spawn(async move {
            callback(service.get_async(relay, policy).await);
        });
        Ok(())
    }

    fn register(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
        permit: Option<OwnedSemaphorePermit>,
    ) -> Result<Registration, RelayInformationError> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.closed {
            return Err(RelayInformationError::ServiceClosed);
        }
        let access = self.shared.access_clock.fetch_add(1, Ordering::Relaxed);
        let entry = state.entries.entry(relay.clone()).or_default();
        entry.last_access = access;
        if policy == RelayInformationCachePolicy::UseCache {
            if let Some(cached) = &entry.cached {
                if now_secs() < cached.fresh_until {
                    let snapshot = cached
                        .snapshot
                        .with_read_state(RelayInformationFreshness::Fresh, None);
                    return Ok(Registration::Ready(Ok(snapshot)));
                }
            }
        }
        if let Some(flight) = entry.flight.as_ref() {
            let generation = flight.generation;
            let receiver = flight.completion.subscribe();
            return Ok(Registration::Flight(FlightWait::new(
                receiver,
                Arc::clone(&self.shared),
                relay,
                generation,
            )));
        }

        let Some(permit) = permit else {
            return Ok(Registration::NeedsAdmission);
        };

        // Reaching this point means the caller owns one of the fixed physical
        // fetch slots. Publish the exact generation before spawning so every
        // racing same-relay caller joins this one completion.
        let generation = self.shared.next_flight.fetch_add(1, Ordering::Relaxed);
        let (cancel_sender, cancel_receiver) = oneshot::channel();
        let cancellation = Arc::new(CancelSignal {
            sender: Mutex::new(Some(cancel_sender)),
        });
        let (completion, receiver) = watch::channel(None);
        entry.flight = Some(Flight {
            generation,
            completion,
            cancellation,
            _permit: permit,
        });
        drop(state);

        let shared = Arc::clone(&self.shared);
        let fetcher = Arc::clone(&self.fetcher);
        let task_relay = relay.clone();
        self.runtime.spawn(async move {
            worker(
                shared,
                task_relay,
                generation,
                fetcher,
                FetchCancellation {
                    receiver: cancel_receiver,
                },
            )
            .await;
        });
        Ok(Registration::Flight(FlightWait::new(
            receiver,
            Arc::clone(&self.shared),
            relay,
            generation,
        )))
    }

    /// Return the current last-good value without initiating I/O.
    pub fn cached(&self, relay: &RelayUrl) -> Option<RelayInformationSnapshot> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let access = self.shared.access_clock.fetch_add(1, Ordering::Relaxed);
        let entry = state.entries.get_mut(relay)?;
        entry.last_access = access;
        let cached = entry.cached.as_ref()?;
        let freshness = if now_secs() < cached.fresh_until {
            RelayInformationFreshness::Fresh
        } else {
            RelayInformationFreshness::Stale
        };
        Some(
            cached
                .snapshot
                .with_read_state(freshness, cached.snapshot.last_error().cloned()),
        )
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(crate) fn retention_census(&self) -> RelayInformationRetentionCensus {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut payloads = std::collections::HashSet::new();
        let mut cached_raw_body_bytes = 0usize;
        let mut cached_entries = 0usize;
        let mut active_flights = 0usize;
        let mut subscribed_callers = 0usize;
        for entry in state.entries.values() {
            if let Some(cached) = &entry.cached {
                cached_entries += 1;
                if payloads.insert(cached.snapshot.payload_identity_value()) {
                    cached_raw_body_bytes =
                        cached_raw_body_bytes.saturating_add(cached.snapshot.raw_json().len());
                }
            }
            if let Some(flight) = &entry.flight {
                active_flights += 1;
                subscribed_callers =
                    subscribed_callers.saturating_add(flight.completion.receiver_count());
            }
        }
        RelayInformationRetentionCensus {
            cached_entries,
            cached_payloads: payloads.len(),
            cached_raw_body_bytes,
            active_flights,
            subscribed_callers,
            max_active_flights: MAX_ACTIVE_FETCHES,
        }
    }

    /// Refuse new acquisition, wake callers awaiting admission, and close
    /// every shared flight completion. Running fetches are signalled
    /// independently; their exact-generation late completion is ignored.
    pub(crate) fn close(&self) {
        self.shared.fetch_slots.close();
        let flights = {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if state.closed {
                return;
            }
            state.closed = true;
            let mut flights = Vec::new();
            for entry in state.entries.values_mut() {
                if let Some(flight) = entry.flight.take() {
                    flights.push(flight);
                }
            }
            state.entries.retain(|_, entry| entry.cached.is_some());
            flights
        };
        for flight in flights {
            flight.cancellation.cancel();
            flight
                .completion
                .send_replace(Some(Err(RelayInformationError::ServiceClosed)));
        }
    }
}

async fn worker(
    shared: Arc<Shared>,
    relay: RelayUrl,
    generation: u64,
    fetcher: Arc<dyn Fetcher>,
    cancellation: FetchCancellation,
) {
    let cached = {
        let state = shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(entry) = state.entries.get(&relay) else {
            return;
        };
        if !entry
            .flight
            .as_ref()
            .is_some_and(|flight| flight.generation == generation)
        {
            return;
        }
        entry.cached.clone()
    };
    let etag = cached
        .as_ref()
        .and_then(|value| value.snapshot.etag())
        .unwrap_or("");
    let last_modified = cached
        .as_ref()
        .and_then(|value| value.snapshot.last_modified())
        .unwrap_or("");
    let validators = (!etag.is_empty() || !last_modified.is_empty())
        .then(|| (etag.to_string(), last_modified.to_string()));
    let result = fetcher
        .fetch_cancellable_async(relay.clone(), validators, cancellation)
        .await
        .and_then(|fetched| finish_fetch(&relay, cached.as_ref(), fetched));
    complete(&shared, &relay, generation, result);
}

fn finish_fetch(
    relay: &RelayUrl,
    cached: Option<&Cached>,
    fetched: FetchResult,
) -> Result<RelayInformationSnapshot, RelayInformationError> {
    if let Some(raw_json) = fetched.raw_json {
        let document = parse_document(&raw_json)?;
        let document_revision = blake3::hash(raw_json.as_bytes()).to_hex().to_string();
        let fresh_for = fetched.fresh_for.unwrap_or(DEFAULT_FRESH_FOR);
        let fetched_at = now_secs();
        let fresh_until = fetched_at.saturating_add(fresh_for.as_secs());
        Ok(RelayInformationSnapshot::new(
            relay.clone(),
            document,
            raw_json,
            document_revision,
            fetched_at,
            fresh_until,
            freshness_at(fresh_until, fetched_at),
            fetched.etag,
            fetched.last_modified,
            fetched.cache_control,
            fetched.expires,
            None,
        ))
    } else {
        let cached = cached.ok_or_else(|| RelayInformationError::Http {
            reason: "relay returned 304 without a cached document".to_string(),
        })?;
        let cache_control = fetched
            .cache_control
            .or_else(|| cached.snapshot.cache_control().map(str::to_owned));
        let expires = fetched
            .expires
            .or_else(|| cached.snapshot.expires().map(str::to_owned));
        let fresh_for = fetched
            .fresh_for
            .or_else(|| {
                fresh_for_headers(
                    cache_control.as_deref(),
                    expires.as_deref(),
                    SystemTime::now(),
                )
            })
            .unwrap_or(DEFAULT_FRESH_FOR);
        let fetched_at = now_secs();
        let fresh_until = fetched_at.saturating_add(fresh_for.as_secs());
        Ok(cached.snapshot.with_metadata(
            fetched_at,
            fresh_until,
            freshness_at(fresh_until, fetched_at),
            fetched
                .etag
                .or_else(|| cached.snapshot.etag().map(str::to_owned)),
            fetched
                .last_modified
                .or_else(|| cached.snapshot.last_modified().map(str::to_owned)),
            cache_control,
            expires,
            None,
        ))
    }
}

fn freshness_at(fresh_until: u64, now: u64) -> RelayInformationFreshness {
    if now < fresh_until {
        RelayInformationFreshness::Fresh
    } else {
        RelayInformationFreshness::Stale
    }
}

enum Registration {
    Ready(Result<RelayInformationSnapshot, RelayInformationError>),
    Flight(FlightWait),
    NeedsAdmission,
}

enum FlightWaitLifecycle {
    Armed,
    Finished,
}

/// Caller-owned subscription to one shared per-relay completion. The service
/// stores only the watch sender; each caller owns its receiver and waiting
/// future. Dropping the final receiver cancels the exact flight and releases
/// its physical admission permit.
struct FlightWait {
    receiver:
        Option<watch::Receiver<Option<Result<RelayInformationSnapshot, RelayInformationError>>>>,
    shared: Arc<Shared>,
    relay: RelayUrl,
    generation: u64,
    lifecycle: FlightWaitLifecycle,
}

impl FlightWait {
    fn new(
        receiver: watch::Receiver<Option<Result<RelayInformationSnapshot, RelayInformationError>>>,
        shared: Arc<Shared>,
        relay: RelayUrl,
        generation: u64,
    ) -> Self {
        Self {
            receiver: Some(receiver),
            shared,
            relay,
            generation,
            lifecycle: FlightWaitLifecycle::Armed,
        }
    }

    async fn wait(mut self) -> Result<RelayInformationSnapshot, RelayInformationError> {
        loop {
            let terminal = self
                .receiver
                .as_ref()
                .and_then(|receiver| receiver.borrow().clone());
            if let Some(result) = terminal {
                self.lifecycle = FlightWaitLifecycle::Finished;
                self.receiver.take();
                return result;
            }
            let changed = self
                .receiver
                .as_mut()
                .expect("an armed flight wait owns its receiver")
                .changed()
                .await;
            if changed.is_err() {
                self.lifecycle = FlightWaitLifecycle::Finished;
                self.receiver.take();
                return Err(RelayInformationError::ServiceClosed);
            }
        }
    }
}

impl Drop for FlightWait {
    fn drop(&mut self) {
        if matches!(self.lifecycle, FlightWaitLifecycle::Armed) {
            self.receiver.take();
            cancel_unobserved_flight(&self.shared, &self.relay, self.generation);
        }
    }
}

fn cancel_unobserved_flight(shared: &Shared, relay: &RelayUrl, generation: u64) {
    let cancellation = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(entry) = state.entries.get_mut(relay) else {
            return;
        };
        let Some(flight) = entry.flight.as_mut() else {
            return;
        };
        if flight.generation != generation {
            return;
        }
        if flight.completion.receiver_count() != 0 {
            return;
        }
        let cancellation = entry
            .flight
            .take()
            .expect("the exact unobserved flight is present")
            .cancellation;
        if entry.cached.is_none() {
            state.entries.remove(relay);
        }
        cancellation
    };
    cancellation.cancel();
}

struct ThreadWake(std::thread::Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Drive one caller-owned future without creating a runtime or helper thread.
/// The HTTP/DNS task itself remains on the engine runtime; this blocks only the
/// synchronous caller that explicitly selected [`RelayInformationService::get`].
fn block_on_caller<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn complete(
    shared: &Shared,
    relay: &RelayUrl,
    generation: u64,
    result: Result<RelayInformationSnapshot, RelayInformationError>,
) {
    let completion = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(entry) = state.entries.get(relay) else {
            return;
        };
        if !entry
            .flight
            .as_ref()
            .is_some_and(|flight| flight.generation == generation)
        {
            return;
        }

        let flight = state
            .entries
            .get_mut(relay)
            .and_then(|entry| entry.flight.take())
            .expect("the exact flight is present");
        let access = shared.access_clock.fetch_add(1, Ordering::Relaxed);
        let delivered = match result {
            Ok(snapshot) => {
                let needs_slot = state
                    .entries
                    .get(relay)
                    .is_none_or(|entry| entry.cached.is_none());
                let mut retain_snapshot = true;
                if needs_slot
                    && state
                        .entries
                        .values()
                        .filter(|entry| entry.cached.is_some())
                        .count()
                        >= shared.cache_capacity
                {
                    // A refreshing entry's last-good snapshot is part of the
                    // true cache cardinality and remains its stale-on-error
                    // authority. Only an idle cached victim is evictable. If
                    // every cached value is refreshing, the fresh completion
                    // is delivered but deliberately not retained.
                    let eviction = state
                        .entries
                        .iter()
                        .filter(|(candidate, entry)| {
                            *candidate != relay && entry.cached.is_some() && entry.flight.is_none()
                        })
                        .min_by_key(|(_, entry)| entry.last_access)
                        .map(|(candidate, _)| candidate.clone());
                    if let Some(eviction) = eviction {
                        state.entries.remove(&eviction);
                    } else {
                        retain_snapshot = false;
                    }
                }
                if retain_snapshot {
                    let entry = state.entries.entry(relay.clone()).or_default();
                    entry.last_access = access;
                    entry.cached = Some(Cached {
                        snapshot: snapshot.clone(),
                        fresh_until: snapshot.fresh_until(),
                    });
                }
                Ok(snapshot)
            }
            Err(error) => {
                let allows_stale = !matches!(
                    error,
                    RelayInformationError::ServiceClosed
                        | RelayInformationError::CredentialedRelayUrl
                );
                match state
                    .entries
                    .entry(relay.clone())
                    .or_default()
                    .cached
                    .as_mut()
                {
                    Some(cached) if allows_stale => {
                        // A failed explicit refresh is new evidence that the
                        // last-good representation cannot keep using its prior
                        // freshness deadline.
                        let stale_at = now_secs();
                        cached.fresh_until = 0;
                        let stale = cached.snapshot.with_metadata(
                            cached.snapshot.fetched_at(),
                            stale_at,
                            RelayInformationFreshness::Stale,
                            cached.snapshot.etag().map(str::to_owned),
                            cached.snapshot.last_modified().map(str::to_owned),
                            cached.snapshot.cache_control().map(str::to_owned),
                            cached.snapshot.expires().map(str::to_owned),
                            Some(error.clone()),
                        );
                        cached.snapshot = stale.clone();
                        Ok(stale)
                    }
                    _ => Err(error),
                }
            }
        };
        state
            .entries
            .retain(|_, entry| entry.cached.is_some() || entry.flight.is_some());
        debug_assert!(
            state
                .entries
                .values()
                .filter(|entry| entry.cached.is_some())
                .count()
                <= shared.cache_capacity
        );
        Some((flight, delivered))
    };
    let Some((flight, delivered)) = completion else {
        return;
    };
    flight.completion.send_replace(Some(delivered));
    // `flight` drops here, releasing the physical fetch permit only after the
    // shared completion has become visible to every subscribed caller.
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Deserialize)]
struct WireDocument {
    name: Option<String>,
    description: Option<String>,
    banner: Option<String>,
    icon: Option<String>,
    pubkey: Option<String>,
    #[serde(rename = "self")]
    self_pubkey: Option<String>,
    contact: Option<String>,
    supported_nips: Option<Vec<u16>>,
    software: Option<String>,
    version: Option<String>,
    terms_of_service: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

fn parse_document(raw_json: &str) -> Result<RelayInformationDocument, RelayInformationError> {
    let wire: WireDocument =
        serde_json::from_str(raw_json).map_err(|error| RelayInformationError::InvalidDocument {
            reason: error.to_string(),
        })?;
    let limitation = wire
        .extra
        .get("limitation")
        .and_then(Value::as_object)
        .map(parse_limitations)
        .unwrap_or_default();
    let structured = wire
        .extra
        .into_iter()
        .filter(|(_, value)| value.is_object() || value.is_array())
        .map(|(key, value)| (key, value.to_string()))
        .collect();
    Ok(RelayInformationDocument {
        name: wire.name,
        description: wire.description,
        banner: wire.banner,
        icon: wire.icon,
        pubkey: wire.pubkey,
        self_pubkey: wire.self_pubkey,
        contact: wire.contact,
        supported_nips: wire.supported_nips,
        software: wire.software,
        version: wire.version,
        terms_of_service: wire.terms_of_service,
        limitation,
        structured,
    })
}

fn parse_limitations(object: &serde_json::Map<String, Value>) -> RelayInformationLimitations {
    let number = |name: &str| object.get(name).and_then(Value::as_u64);
    let boolean = |name: &str| object.get(name).and_then(Value::as_bool);
    RelayInformationLimitations {
        max_message_length: number("max_message_length"),
        max_subscriptions: number("max_subscriptions"),
        max_filters: number("max_filters"),
        max_limit: number("max_limit"),
        max_subid_length: number("max_subid_length"),
        max_event_tags: number("max_event_tags"),
        max_content_length: number("max_content_length"),
        min_pow_difficulty: number("min_pow_difficulty"),
        auth_required: boolean("auth_required"),
        payment_required: boolean("payment_required"),
        created_at_lower_limit: number("created_at_lower_limit"),
        created_at_upper_limit: number("created_at_upper_limit"),
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};
    use std::thread::JoinHandle;
    use std::time::Instant;

    use super::*;
    /// This whole test module's standing convention (long predating issue
    /// #519) is a real `TcpListener::bind("127.0.0.1:0")` standing in for a
    /// relay's HTTP endpoint. That is exactly the shape issue #519's
    /// resolved-IP admission check now refuses by default, so every test
    /// below that fetches from such a listener needs its host explicitly
    /// opted in — precisely the "don't break the intentional local-relay
    /// path" requirement, applied to this crate's own test doubles rather
    /// than a real operator config.
    fn loopback_admission() -> Arc<BTreeSet<String>> {
        Arc::new(BTreeSet::from([
            "127.0.0.1".to_string(),
            "::1".to_string(),
            "localhost".to_string(),
        ]))
    }

    fn resolver_config_for_dns_server(
        address: std::net::SocketAddr,
    ) -> hickory_resolver::config::ResolverConfig {
        let mut udp = hickory_resolver::config::ConnectionConfig::udp();
        udp.port = address.port();
        let mut tcp = hickory_resolver::config::ConnectionConfig::tcp();
        tcp.port = address.port();
        let nameserver =
            hickory_resolver::config::NameServerConfig::new(address.ip(), true, vec![udp, tcp]);
        hickory_resolver::config::ResolverConfig::from_parts(None, Vec::new(), vec![nameserver])
    }

    fn local_relay_information_service(runtime: tokio::runtime::Handle) -> RelayInformationService {
        RelayInformationService::with_runtime_and_limits(
            runtime,
            Arc::new(HttpFetcher::new_with_admission(loopback_admission())),
            CACHE_CAPACITY,
        )
    }

    /// A small multi-thread runtime for tests that drive real flights. Four
    /// workers keep several concurrently gated flights (whose test fetchers
    /// block a worker thread) from deadlocking. Callers keep it alive for the
    /// test's duration and `drop` it in place of the old `executor.shutdown()`.
    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap()
    }

    /// Test-only caller adapter for legacy synchronous assertions. The task is
    /// owned by the test caller; production service state still contains only
    /// the shared flight completion and fixed physical fetch permits.
    fn spawn_test_get(
        service: RelayInformationService,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
    ) -> Receiver<Result<RelayInformationSnapshot, RelayInformationError>> {
        let (reply, receiver) = bounded(1);
        service.runtime.clone().spawn(async move {
            let _ = reply.send(service.get_async(relay, policy).await);
        });
        receiver
    }

    struct CountingFetcher {
        calls: AtomicUsize,
        fail_after_first: bool,
    }

    struct GatedFetcher {
        started: Sender<()>,
        release: Receiver<()>,
    }

    struct HoldingFetcher {
        active: Arc<AtomicUsize>,
        maximum: Arc<AtomicUsize>,
    }

    struct ActiveFetchGuard(Arc<AtomicUsize>);

    impl Drop for ActiveFetchGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Fetcher for HoldingFetcher {
        fn fetch_cancellable_async<'a>(
            &'a self,
            _relay: RelayUrl,
            _validators: Option<(String, String)>,
            cancellation: FetchCancellation,
        ) -> Pin<Box<dyn Future<Output = Result<FetchResult, RelayInformationError>> + Send + 'a>>
        {
            let active = Arc::clone(&self.active);
            let maximum = Arc::clone(&self.maximum);
            Box::pin(async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(current, Ordering::SeqCst);
                let _guard = ActiveFetchGuard(active);
                let _ = cancellation.receiver.await;
                Err(RelayInformationError::ServiceClosed)
            })
        }
    }

    struct MalformedThenGoodFetcher {
        calls: AtomicUsize,
    }

    impl Fetcher for GatedFetcher {
        fn fetch_cancellable_async<'a>(
            &'a self,
            _relay: RelayUrl,
            _validators: Option<(String, String)>,
            _cancellation: FetchCancellation,
        ) -> Pin<Box<dyn Future<Output = Result<FetchResult, RelayInformationError>> + Send + 'a>>
        {
            let started = self.started.clone();
            let release = self.release.clone();
            Box::pin(async move {
                let _ = started.send(());
                // Deliberately a blocking recv: this test double stands in for
                // an HTTP worker stuck mid-request, occupying a runtime worker
                // thread until its harness releases it.
                release
                    .recv()
                    .map_err(|_| RelayInformationError::ServiceClosed)?;
                Ok(FetchResult {
                    raw_json: Some(r#"{"name":"Async"}"#.to_string()),
                    etag: None,
                    last_modified: None,
                    cache_control: None,
                    expires: None,
                    fresh_for: Some(DEFAULT_FRESH_FOR),
                })
            })
        }
    }

    struct ChannelWake(std::sync::mpsc::Sender<()>);

    fn read_http_headers(stream: &mut std::net::TcpStream) {
        let mut received = Vec::new();
        let mut buffer = [0u8; 1024];
        while !received.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0, "HTTP request ended before its headers");
            received.extend_from_slice(&buffer[..count]);
        }
    }

    impl Wake for ChannelWake {
        fn wake(self: Arc<Self>) {
            let _ = self.0.send(());
        }

        fn wake_by_ref(self: &Arc<Self>) {
            let _ = self.0.send(());
        }
    }

    impl Fetcher for CountingFetcher {
        fn fetch_cancellable_async<'a>(
            &'a self,
            _relay: RelayUrl,
            _validators: Option<(String, String)>,
            _cancellation: FetchCancellation,
        ) -> Pin<Box<dyn Future<Output = Result<FetchResult, RelayInformationError>> + Send + 'a>>
        {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let fail_after_first = self.fail_after_first;
            Box::pin(async move {
                if fail_after_first && call > 0 {
                    return Err(RelayInformationError::Http {
                        reason: "offline".to_string(),
                    });
                }
                Ok(FetchResult {
                    raw_json: Some(
                        r#"{"name":"Example","supported_nips":[11,50,77],"limitation":{"max_subscriptions":20,"max_limit":500,"auth_required":true,"future_limit":"kept"},"future":{"x":1}}"#.to_string(),
                    ),
                    etag: Some("v1".to_string()),
                    last_modified: None,
                    cache_control: Some("max-age=3600".to_string()),
                    expires: None,
                    fresh_for: Some(DEFAULT_FRESH_FOR),
                })
            })
        }
    }

    impl Fetcher for MalformedThenGoodFetcher {
        fn fetch_cancellable_async<'a>(
            &'a self,
            _relay: RelayUrl,
            _validators: Option<(String, String)>,
            _cancellation: FetchCancellation,
        ) -> Pin<Box<dyn Future<Output = Result<FetchResult, RelayInformationError>> + Send + 'a>>
        {
            let raw_json = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                "not-json"
            } else {
                r#"{"name":"Recovered"}"#
            };
            Box::pin(async move {
                Ok(FetchResult {
                    raw_json: Some(raw_json.to_string()),
                    etag: None,
                    last_modified: None,
                    cache_control: None,
                    expires: None,
                    fresh_for: Some(DEFAULT_FRESH_FOR),
                })
            })
        }
    }

    #[test]
    fn concurrent_requests_share_one_flight_and_preserve_raw_json() {
        let (started_tx, started_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let service = RelayInformationService::try_with_fetcher(Arc::new(GatedFetcher {
            started: started_tx,
            release: release_rx,
        }))
        .unwrap();
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let canonical_equivalent = RelayUrl::parse("wss://relay.example/").unwrap();
        assert_eq!(relay, canonical_equivalent);
        let a = spawn_test_get(
            service.clone(),
            relay.clone(),
            RelayInformationCachePolicy::Refresh,
        );
        started_rx.recv().unwrap();
        let b = spawn_test_get(
            service.clone(),
            canonical_equivalent,
            RelayInformationCachePolicy::Refresh,
        );
        let subscription_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while service.retention_census().subscribed_callers != 2 {
            assert!(
                std::time::Instant::now() < subscription_deadline,
                "the second caller subscribed to the shared flight"
            );
            std::thread::yield_now();
        }
        release_tx.send(()).unwrap();
        let a = a.recv().unwrap().unwrap();
        let b = b.recv().unwrap().unwrap();
        assert_eq!(a, b);
        assert_eq!(a.payload_identity(), b.payload_identity());
        assert_eq!(a.document().name.as_deref(), Some("Async"));
        assert_eq!(a.document_revision().len(), 64);
    }

    #[test]
    fn async_cold_miss_suspends_while_http_worker_is_blocked() {
        let (started_tx, started_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let service = RelayInformationService::try_with_fetcher(Arc::new(GatedFetcher {
            started: started_tx,
            release: release_rx,
        }))
        .unwrap();
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let mut future = Box::pin(service.get_async(relay, RelayInformationCachePolicy::Refresh));
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let waker = Waker::from(Arc::new(ChannelWake(wake_tx)));
        let mut context = Context::from_waker(&waker);

        assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the HTTP worker started while the caller remained suspended");

        release_tx.send(()).unwrap();
        wake_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("completion wakes the suspended caller");
        let snapshot = match future.as_mut().poll(&mut context) {
            Poll::Ready(Ok(snapshot)) => snapshot,
            other => panic!("expected a completed async snapshot, got {other:?}"),
        };
        assert_eq!(snapshot.document().name.as_deref(), Some("Async"));
    }

    #[test]
    fn malformed_first_response_does_not_poison_future_attempts() {
        let fetcher = Arc::new(MalformedThenGoodFetcher {
            calls: AtomicUsize::new(0),
        });
        let service = RelayInformationService::try_with_fetcher(fetcher.clone()).unwrap();
        let relay = RelayUrl::parse("wss://relay.example").unwrap();

        assert!(matches!(
            service.get(relay.clone(), RelayInformationCachePolicy::UseCache),
            Err(RelayInformationError::InvalidDocument { .. })
        ));
        let recovered = service
            .get(relay, RelayInformationCachePolicy::UseCache)
            .unwrap();
        assert_eq!(recovered.document().name.as_deref(), Some("Recovered"));
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn refresh_failure_returns_stale_last_good_with_separate_error() {
        let fetcher = Arc::new(CountingFetcher {
            calls: AtomicUsize::new(0),
            fail_after_first: true,
        });
        let service = RelayInformationService::try_with_fetcher(fetcher.clone()).unwrap();
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let fresh = service
            .get(relay.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        let stale = service
            .get(relay.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        assert_eq!(fresh.payload_identity(), stale.payload_identity());
        assert_eq!(fresh.raw_json().as_ptr(), stale.raw_json().as_ptr());
        assert_eq!(fresh.document() as *const _, stale.document() as *const _);
        assert_eq!(
            fresh.document_revision().as_ptr(),
            stale.document_revision().as_ptr()
        );
        assert_eq!(stale.document().name.as_deref(), Some("Example"));
        assert_eq!(stale.freshness(), RelayInformationFreshness::Stale);
        assert!(matches!(
            stale.last_error(),
            Some(RelayInformationError::Http { .. })
        ));

        let still_stale = service
            .get(relay, RelayInformationCachePolicy::UseCache)
            .unwrap();
        assert_eq!(still_stale.freshness(), RelayInformationFreshness::Stale);
        assert!(matches!(
            still_stale.last_error(),
            Some(RelayInformationError::Http { .. })
        ));
        assert_eq!(
            fetcher.calls.load(Ordering::SeqCst),
            3,
            "a failed refresh must expire the old TTL, so UseCache retries instead of laundering stale evidence"
        );
    }

    #[test]
    fn use_cache_returns_a_fresh_value_without_another_fetch() {
        let fetcher = Arc::new(CountingFetcher {
            calls: AtomicUsize::new(0),
            fail_after_first: false,
        });
        let service = RelayInformationService::try_with_fetcher(fetcher.clone()).unwrap();
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let first = service
            .get(relay.clone(), RelayInformationCachePolicy::UseCache)
            .unwrap();
        let second = service
            .get(relay, RelayInformationCachePolicy::UseCache)
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cache_evicts_the_least_recently_used_relay_at_its_bound() {
        let fetcher = Arc::new(CountingFetcher {
            calls: AtomicUsize::new(0),
            fail_after_first: false,
        });
        let service = RelayInformationService::with_fetcher_and_capacity(fetcher, 2).unwrap();
        let first = RelayUrl::parse("wss://first.example").unwrap();
        let second = RelayUrl::parse("wss://second.example").unwrap();
        let third = RelayUrl::parse("wss://third.example").unwrap();

        service
            .get(first.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        service
            .get(second.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        service
            .get(third.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();

        assert!(service.cached(&first).is_none());
        assert!(service.cached(&second).is_some());
        assert!(service.cached(&third).is_some());
    }

    #[test]
    fn redirect_is_rejected_without_contacting_its_target() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let target_addr = target.local_addr().unwrap();
        let redirect = TcpListener::bind("127.0.0.1:0").unwrap();
        let redirect_addr = redirect.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = redirect.accept().unwrap();
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let relay = RelayUrl::parse(&format!("ws://{redirect_addr}")).unwrap();
        let error = match HttpFetcher::new_with_admission(loopback_admission()).fetch(&relay, None)
        {
            Err(error) => error,
            Ok(_) => panic!("a redirect must not be accepted as NIP-11 data"),
        };
        server.join().unwrap();

        assert!(matches!(error, RelayInformationError::Http { .. }));
        assert!(matches!(
            target.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn cache_control_no_cache_is_retained_and_never_labeled_fresh() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_headers(&mut stream);
            let body = r#"{"name":"No cache"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let relay = RelayUrl::parse(&format!("ws://{address}")).unwrap();
        let rt = test_runtime();
        let service = local_relay_information_service(rt.handle().clone());
        let value = service
            .get(relay.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        server.join().unwrap();

        assert_eq!(value.cache_control(), Some("no-cache"));
        assert!(value.fresh_until() <= value.fetched_at());
        assert_eq!(value.freshness(), RelayInformationFreshness::Stale);
        assert_eq!(
            service.cached(&relay).unwrap().freshness(),
            RelayInformationFreshness::Stale
        );
        service.close();
        drop(rt);
    }

    #[test]
    fn expires_header_sets_and_preserves_the_freshness_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let expires_at = SystemTime::now() + Duration::from_secs(120);
        let expires = httpdate::fmt_http_date(expires_at);
        let expected_expires = expires.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_headers(&mut stream);
            let body = r#"{"name":"Expiring"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nExpires: {expires}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let relay = RelayUrl::parse(&format!("ws://{address}")).unwrap();
        let rt = test_runtime();
        let service = local_relay_information_service(rt.handle().clone());
        let value = service
            .get(relay, RelayInformationCachePolicy::Refresh)
            .unwrap();
        server.join().unwrap();

        assert_eq!(value.expires(), Some(expected_expires.as_str()));
        assert!(value.fresh_until().saturating_sub(value.fetched_at()) >= 115);
        assert!(value.fresh_until().saturating_sub(value.fetched_at()) <= 120);
        service.close();
        drop(rt);
    }

    #[test]
    fn past_expires_and_zero_fresh_304_are_stale_at_delivery() {
        let relay = RelayUrl::parse("wss://stale.example").unwrap();
        let past = httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(1));
        let past_fresh_for = fresh_for_headers(None, Some(&past), SystemTime::now()).unwrap();
        assert_eq!(past_fresh_for, Duration::ZERO);
        let first = finish_fetch(
            &relay,
            None,
            FetchResult {
                raw_json: Some(r#"{"name":"Past"}"#.to_string()),
                etag: Some("v1".to_string()),
                last_modified: None,
                cache_control: None,
                expires: Some(past),
                fresh_for: Some(past_fresh_for),
            },
        )
        .unwrap();
        assert_eq!(first.freshness(), RelayInformationFreshness::Stale);
        let first_payload = first.payload_identity();
        let first_raw = first.raw_json().as_ptr();
        let first_document = first.document() as *const _;
        let first_revision = first.document_revision().as_ptr();

        let cached = Cached {
            fresh_until: first.fresh_until(),
            snapshot: first,
        };
        let revalidated = finish_fetch(
            &relay,
            Some(&cached),
            FetchResult {
                raw_json: None,
                etag: Some("v1".to_string()),
                last_modified: None,
                cache_control: Some("no-cache".to_string()),
                expires: None,
                fresh_for: Some(Duration::ZERO),
            },
        )
        .unwrap();
        assert_eq!(revalidated.payload_identity(), first_payload);
        assert_eq!(revalidated.raw_json().as_ptr(), first_raw);
        assert_eq!(revalidated.document() as *const _, first_document);
        assert_eq!(revalidated.document_revision().as_ptr(), first_revision);
        assert_eq!(revalidated.freshness(), RelayInformationFreshness::Stale);
        assert!(revalidated.fresh_until() <= revalidated.fetched_at());
    }

    #[test]
    fn hostile_max_age_saturates_without_panicking_and_the_flight_can_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for (name, cache_control) in [
                ("Maximum", "max-age=18446744073709551615"),
                ("Retried", "max-age=60"),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                read_http_headers(&mut stream);
                let body = format!(r#"{{"name":"{name}"}}"#);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nCache-Control: {cache_control}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        let relay = RelayUrl::parse(&format!("ws://{address}")).unwrap();
        let rt = test_runtime();
        let service = local_relay_information_service(rt.handle().clone());

        let maximum = service
            .get(relay.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        assert_eq!(maximum.fresh_until(), u64::MAX);
        let retried = service
            .get(relay, RelayInformationCachePolicy::Refresh)
            .unwrap();
        assert_eq!(retried.document().name.as_deref(), Some("Retried"));

        server.join().unwrap();
        service.close();
        drop(rt);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn distinct_fetch_overload_has_a_finite_physical_envelope_and_cache_hits_progress() {
        const CALLERS: usize = MAX_ACTIVE_FETCHES * 4;
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let service = RelayInformationService::try_with_fetcher(Arc::new(HoldingFetcher {
            active: Arc::clone(&active),
            maximum: Arc::clone(&maximum),
        }))
        .unwrap();

        let cached_relay = RelayUrl::parse("wss://cached-progress.example").unwrap();
        let cached_snapshot = finish_fetch(
            &cached_relay,
            None,
            FetchResult {
                raw_json: Some(r#"{"name":"cache still progresses"}"#.to_string()),
                etag: None,
                last_modified: None,
                cache_control: None,
                expires: None,
                fresh_for: Some(DEFAULT_FRESH_FOR),
            },
        )
        .unwrap();
        service.shared.state.lock().unwrap().entries.insert(
            cached_relay.clone(),
            Entry {
                cached: Some(Cached {
                    fresh_until: cached_snapshot.fresh_until(),
                    snapshot: cached_snapshot,
                }),
                flight: None,
                last_access: 0,
            },
        );

        let mut callers = Vec::new();
        for index in 0..CALLERS {
            let service = service.clone();
            callers.push(tokio::spawn(async move {
                service
                    .get_async(
                        RelayUrl::parse(&format!("wss://held-{index}.example")).unwrap(),
                        RelayInformationCachePolicy::Refresh,
                    )
                    .await
            }));
        }

        tokio::time::timeout(Duration::from_secs(2), async {
            while active.load(Ordering::SeqCst) != MAX_ACTIVE_FETCHES {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the fixed physical NIP-11 envelope becomes fully occupied");

        let census = service.retention_census();
        assert_eq!(census.active_flights, MAX_ACTIVE_FETCHES);
        assert_eq!(census.max_active_flights, MAX_ACTIVE_FETCHES);
        assert_eq!(census.subscribed_callers, MAX_ACTIVE_FETCHES);
        assert_eq!(maximum.load(Ordering::SeqCst), MAX_ACTIVE_FETCHES);

        let cached = tokio::time::timeout(
            Duration::from_millis(100),
            service.get_async(cached_relay, RelayInformationCachePolicy::UseCache),
        )
        .await
        .expect("an unrelated cache hit progresses while every fetch slot is occupied")
        .unwrap();
        assert_eq!(
            cached.document().name.as_deref(),
            Some("cache still progresses")
        );

        for caller in &callers {
            caller.abort();
        }
        for caller in callers {
            let _ = caller.await;
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while service.retention_census().active_flights != 0
                || active.load(Ordering::SeqCst) != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelling admitted and admission-waiting callers releases every flight");
    }

    #[test]
    fn dropping_the_last_async_waiter_cancels_its_exact_generation() {
        let (started_tx, started_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let rt = test_runtime();
        let service = RelayInformationService::with_runtime_and_limits(
            rt.handle().clone(),
            Arc::new(GatedFetcher {
                started: started_tx,
                release: release_rx,
            }),
            2,
        );
        let relay = RelayUrl::parse("wss://cancelled.example").unwrap();
        let mut future =
            Box::pin(service.get_async(relay.clone(), RelayInformationCachePolicy::Refresh));
        let waker = Waker::from(Arc::new(ChannelWake(std::sync::mpsc::channel().0)));
        let mut context = Context::from_waker(&waker);
        assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
        started_rx.recv().unwrap();
        drop(future);
        assert!(!service
            .shared
            .state
            .lock()
            .unwrap()
            .entries
            .contains_key(&relay));
        // Release the gated worker so its runtime task can finish before the
        // runtime is dropped (the fetcher blocks on this recv, ignoring the
        // generation cancellation the dropped waiter fired).
        release_tx.send(()).unwrap();
        drop(rt);
    }

    #[test]
    fn late_old_generation_cannot_overwrite_or_drain_the_new_flight() {
        let relay = RelayUrl::parse("wss://generation.example").unwrap();
        let fetch_slots = Arc::new(Semaphore::new(MAX_ACTIVE_FETCHES));
        let permit = Arc::clone(&fetch_slots).try_acquire_owned().unwrap();
        let (completion, receiver) = watch::channel(None);
        let (cancel, _cancelled) = oneshot::channel();
        let mut entries = HashMap::new();
        entries.insert(
            relay.clone(),
            Entry {
                cached: None,
                flight: Some(Flight {
                    generation: 2,
                    completion,
                    cancellation: Arc::new(CancelSignal {
                        sender: Mutex::new(Some(cancel)),
                    }),
                    _permit: permit,
                }),
                last_access: 0,
            },
        );
        let shared = Shared {
            state: Mutex::new(State {
                closed: false,
                entries,
            }),
            access_clock: AtomicU64::new(0),
            next_flight: AtomicU64::new(3),
            cache_capacity: 2,
            fetch_slots,
        };
        let old = finish_fetch(
            &relay,
            None,
            FetchResult {
                raw_json: Some(r#"{"name":"old"}"#.to_string()),
                etag: None,
                last_modified: None,
                cache_control: None,
                expires: None,
                fresh_for: Some(DEFAULT_FRESH_FOR),
            },
        )
        .unwrap();
        let old_payload = Arc::downgrade(&old.inner.payload);
        complete(&shared, &relay, 1, Ok(old));
        assert!(
            old_payload.upgrade().is_none(),
            "an ignored late generation must not retain its immutable payload"
        );
        assert!(receiver.borrow().is_none());
        assert_eq!(
            shared
                .state
                .lock()
                .unwrap()
                .entries
                .get(&relay)
                .unwrap()
                .flight
                .as_ref()
                .unwrap()
                .generation,
            2
        );

        let new = finish_fetch(
            &relay,
            None,
            FetchResult {
                raw_json: Some(r#"{"name":"new"}"#.to_string()),
                etag: None,
                last_modified: None,
                cache_control: None,
                expires: None,
                fresh_for: Some(DEFAULT_FRESH_FOR),
            },
        )
        .unwrap();
        complete(&shared, &relay, 2, Ok(new));
        assert_eq!(
            receiver
                .borrow()
                .clone()
                .expect("the current generation completes")
                .unwrap()
                .document()
                .name
                .as_deref(),
            Some("new")
        );
    }

    #[test]
    fn retained_service_clone_cannot_hold_or_reopen_an_http_task_after_close() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(std::sync::Barrier::new(2));
        let server_accepted = Arc::clone(&accepted);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_headers(&mut stream);
            server_accepted.wait();
            let mut sink = Vec::new();
            let _ = stream.read_to_end(&mut sink);
        });
        let rt = test_runtime();
        let service = local_relay_information_service(rt.handle().clone());
        let retained = service.clone();
        let relay = RelayUrl::parse(&format!("ws://{address}")).unwrap();
        let receiver = spawn_test_get(
            service.clone(),
            relay.clone(),
            RelayInformationCachePolicy::Refresh,
        );
        accepted.wait();

        let started = Instant::now();
        service.close();
        assert_eq!(
            receiver.recv().unwrap(),
            Err(RelayInformationError::ServiceClosed)
        );
        drop(rt);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(matches!(
            retained.get(relay, RelayInformationCachePolicy::Refresh),
            Err(RelayInformationError::ServiceClosed)
        ));
        server.join().unwrap();
    }

    #[test]
    fn slow_drip_body_is_stopped_by_the_total_request_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_headers(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n")
                .unwrap();
            for _ in 0..20 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        });
        let relay = RelayUrl::parse(&format!("ws://{address}")).unwrap();
        let started = Instant::now();
        let error = HttpFetcher::new_with_admission(loopback_admission())
            .fetch(&relay, None)
            .unwrap_err();
        assert!(matches!(error, RelayInformationError::Http { .. }));
        assert!(started.elapsed() < Duration::from_secs(5));
        server.join().unwrap();
    }

    #[test]
    fn non_success_is_one_request_with_no_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_headers(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            listener.set_nonblocking(true).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            assert!(matches!(
                listener.accept(),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
            ));
        });
        let relay = RelayUrl::parse(&format!("ws://{address}")).unwrap();
        assert!(matches!(
            HttpFetcher::new_with_admission(loopback_admission()).fetch(&relay, None),
            Err(RelayInformationError::Http { .. })
        ));
        server.join().unwrap();
    }

    /// Spin up a fake authoritative DNS server that answers every A query
    /// for `relay.nmp.test` with `127.0.0.1` (60-second TTL). Shared by the
    /// opted-in-success and refused-by-default falsifiers below (issue
    /// #519) — this DNS-injection harness itself was the confirmed exploit
    /// surface (the fetch used to just... work).
    fn spawn_loopback_dns() -> (hickory_resolver::config::ResolverConfig, JoinHandle<()>) {
        let dns = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let dns_address = dns.local_addr().unwrap();
        let dns_server = std::thread::spawn(move || {
            let mut query = [0u8; 512];
            let (length, peer) = dns.recv_from(&mut query).unwrap();
            assert!(length > 16);
            let mut cursor = 12;
            while query[cursor] != 0 {
                cursor += query[cursor] as usize + 1;
            }
            cursor += 1;
            assert_eq!(u16::from_be_bytes([query[cursor], query[cursor + 1]]), 1);
            let question_end = cursor + 4;
            let mut response = Vec::new();
            response.extend_from_slice(&query[..2]);
            response.extend_from_slice(&[0x81, 0x80]);
            response.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 0]);
            response.extend_from_slice(&query[12..question_end]);
            response.extend_from_slice(&[
                0xc0, 0x0c, // compressed owner name
                0x00, 0x01, // A
                0x00, 0x01, // IN
                0x00, 0x00, 0x00, 0x3c, // 60-second TTL
                0x00, 0x04, 127, 0, 0, 1,
            ]);
            dns.send_to(&response, peer).unwrap();
        });
        let resolver = resolver_config_for_dns_server(dns_address);
        (resolver, dns_server)
    }

    /// This test used to be the confirmed exploit (issue #519): a DNS answer
    /// pointing `relay.nmp.test` at `127.0.0.1` let the NIP-11 fetch reach a
    /// loopback listener with no opt-in at all. Now that
    /// `HickoryReqwestResolver::resolve` refuses unopted-in `Local`
    /// addresses, this exact scenario only still succeeds because the
    /// fetcher is explicitly constructed with `relay.nmp.test` opted in —
    /// pinning issue #519's "don't break the intentional local-relay path"
    /// requirement using the SAME resolver-injection harness the original
    /// exploit used.
    #[test]
    fn opted_in_host_still_resolves_and_fetches_through_hickory() {
        let (resolver, dns_server) = spawn_loopback_dns();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_headers(&mut stream);
            let body = r#"{"name":"Hostname"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let relay = RelayUrl::parse(&format!("ws://relay.nmp.test:{port}")).unwrap();
        let allowed = Arc::new(BTreeSet::from(["relay.nmp.test".to_string()]));
        let value = HttpFetcher::with_resolver_config_and_admission(resolver, allowed)
            .fetch(&relay, None)
            .unwrap();
        assert!(value.raw_json.is_some_and(|json| json.contains("Hostname")));
        dns_server.join().unwrap();
        server.join().unwrap();
    }

    /// issue #519 (HIGH) falsifier: the exact same DNS-to-loopback answer,
    /// with NO opt-in, must now be refused rather than silently fetched.
    /// Deliberately no HTTP listener at all here: a correct fix refuses the
    /// resolved address before reqwest ever attempts to dial it, so there is
    /// nothing for a listener to accept.
    #[test]
    fn dns_resolution_to_loopback_is_refused_without_opt_in() {
        let (resolver, dns_server) = spawn_loopback_dns();
        let relay = RelayUrl::parse("ws://relay.nmp.test:80").unwrap();
        let error = HttpFetcher::with_resolver_config(resolver)
            .fetch(&relay, None)
            .unwrap_err();
        assert!(
            matches!(error, RelayInformationError::Http { .. }),
            "expected a refused/failed HTTP fetch, got {error:?}"
        );
        dns_server.join().unwrap();
    }

    #[test]
    fn held_hickory_dns_is_cancelled_and_joined_at_exact_shutdown() {
        let dns = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let dns_address = dns.local_addr().unwrap();
        let (query_seen_tx, query_seen_rx) = bounded(1);
        let (release_dns_tx, release_dns_rx) = bounded(1);
        let dns_server = std::thread::spawn(move || {
            let mut query = [0u8; 512];
            let _ = dns.recv_from(&mut query).unwrap();
            query_seen_tx.send(()).unwrap();
            let _ = release_dns_rx.recv();
        });
        let resolver = resolver_config_for_dns_server(dns_address);
        let rt = test_runtime();
        let service = RelayInformationService::with_runtime_and_limits(
            rt.handle().clone(),
            Arc::new(HttpFetcher::with_resolver_config(resolver)),
            2,
        );
        let relay = RelayUrl::parse("ws://held-dns.nmp.test:80").unwrap();
        let result = spawn_test_get(service.clone(), relay, RelayInformationCachePolicy::Refresh);
        query_seen_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the injected Hickory server observes the unresolved query");

        let started = Instant::now();
        service.close();
        assert!(matches!(
            result.recv_timeout(Duration::from_secs(1)),
            Ok(Err(RelayInformationError::ServiceClosed))
        ));
        // Dropping the runtime joins the cancelled fetch task; the held DNS
        // query's future is dropped when cancellation fires, so teardown stays
        // inside the public <5s lifecycle bound.
        drop(rt);
        assert!(started.elapsed() < Duration::from_secs(5));

        release_dns_tx.send(()).unwrap();
        dns_server.join().unwrap();
    }

    // #704: deleted — asserted the internal per-fetch `http_runtime()` was a
    // single-worker current-thread runtime with no tokio worker pool. That
    // per-fetch runtime is gone; fetches now run as async tasks on the shared
    // engine-owned multi-thread runtime, so the invariant no longer applies.

    #[test]
    fn websocket_urls_map_to_http_without_losing_path() {
        assert_eq!(
            relay_http_url(&RelayUrl::parse("wss://relay.example/nostr").unwrap())
                .unwrap()
                .0
                .as_str(),
            "https://relay.example/nostr"
        );
    }

    #[test]
    fn relay_url_userinfo_is_typed_refusal_before_any_request_or_authorization_header() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();

        for userinfo in ["user:secret@", "user@", ":secret@"] {
            let relay = RelayUrl::parse(&format!("ws://{userinfo}{address}/nip11")).unwrap();
            assert!(matches!(
                HttpFetcher::new_with_admission(loopback_admission()).fetch(&relay, None),
                Err(RelayInformationError::CredentialedRelayUrl)
            ));
        }

        // `RelayUrl::parse` normalizes an empty userinfo marker away. The
        // resulting typed URL therefore carries no credential that reqwest
        // could project as Basic Authorization.
        let empty = RelayUrl::parse(&format!("ws://@{address}/nip11")).unwrap();
        let normalized: &reqwest::Url = (&empty).into();
        assert!(normalized.username().is_empty());
        assert!(normalized.password().is_none());

        std::thread::sleep(Duration::from_millis(50));
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }
}
