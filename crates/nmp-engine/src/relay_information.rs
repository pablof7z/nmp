//! Engine-owned, one-shot NIP-11 acquisition.
//!
//! NIP-11 is HTTP state, not a reactive stream. This service gives callers
//! an explicit one-shot read while sharing a bounded, in-memory cache and a
//! per-relay single flight. The last good document is retained separately
//! from the last acquisition error, so a transient failure never destroys
//! useful presentation or capability evidence.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{bounded, Receiver, Sender};
use futures_channel::oneshot;
use nostr::RelayUrl;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_FRESH_FOR: Duration = Duration::from_secs(60 * 60);
// Engine teardown has a public <5s lifecycle falsifier. This is an overall
// request deadline (headers and body), not a per-read timeout, so a peer that
// accepts a connection and then stops responding cannot hold shutdown past
// that contract.
const FETCH_DEADLINE: Duration = Duration::from_secs(3);
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
const CACHE_CAPACITY: usize = 256;
const WAITER_CAPACITY: usize = 64;

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
    ExecutorSaturated { capacity: usize },
    WaiterSaturated { capacity: usize },
    ThreadUnavailable { reason: String },
    ServiceClosed,
    Http { reason: String },
    ResponseTooLarge { limit_bytes: u64 },
    InvalidDocument { reason: String },
}

impl std::fmt::Display for RelayInformationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExecutorSaturated { capacity } => write!(
                f,
                "NIP-11 acquisition refused: native task capacity {capacity} is full"
            ),
            Self::WaiterSaturated { capacity } => write!(
                f,
                "NIP-11 acquisition refused: per-relay waiter capacity {capacity} is full"
            ),
            Self::ThreadUnavailable { reason } => {
                write!(f, "NIP-11 acquisition thread unavailable: {reason}")
            }
            Self::ServiceClosed => f.write_str("NIP-11 acquisition service is closed"),
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
#[derive(Debug, Clone, PartialEq)]
pub struct RelayInformationSnapshot {
    pub relay: RelayUrl,
    pub document: RelayInformationDocument,
    pub raw_json: String,
    /// Stable BLAKE3 identity of the exact received JSON representation.
    /// Capability facts cite this revision rather than an unscoped boolean.
    pub document_revision: String,
    pub fetched_at: u64,
    pub fresh_until: u64,
    pub freshness: RelayInformationFreshness,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Raw HTTP freshness directives retained for inspection and later
    /// persistence. Their interpreted deadline is `fresh_until`.
    pub cache_control: Option<String>,
    pub expires: Option<String>,
    /// A stale-on-error result carries the failure here without replacing
    /// the last good document. Fresh successful results always have `None`.
    pub last_error: Option<RelayInformationError>,
}

impl RelayInformationSnapshot {
    /// Advertisement only. This never creates a behavioral capability token.
    pub fn advertises_nip(&self, nip: u16) -> Option<bool> {
        self.document
            .supported_nips
            .as_ref()
            .map(|nips| nips.contains(&nip))
    }

    pub(crate) fn capability_evidence(&self) -> RelayInformationCapabilityEvidence {
        RelayInformationCapabilityEvidence {
            supported_nips: self.document.supported_nips.clone(),
            document_revision: self.document_revision.clone(),
            freshness: self.freshness,
            last_error: self.last_error.clone(),
        }
    }
}

/// The provenance-bearing subset of a NIP-11 snapshot used by engine
/// capability decisions and diagnostics. It deliberately excludes runtime
/// connection/AUTH state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayInformationCapabilityEvidence {
    pub supported_nips: Option<Vec<u16>>,
    pub document_revision: String,
    pub freshness: RelayInformationFreshness,
    pub last_error: Option<RelayInformationError>,
}

#[derive(Clone)]
pub struct RelayInformationService {
    shared: Arc<Shared>,
    executor: nmp_executor::Executor,
    fetcher: Arc<dyn Fetcher>,
}

struct Shared {
    state: Mutex<State>,
    access_clock: AtomicU64,
    next_flight: AtomicU64,
    next_waiter: AtomicU64,
    cache_capacity: usize,
    waiter_capacity: usize,
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
    waiters: Vec<Waiter>,
    cancellation: Arc<CancelSignal>,
}

struct Waiter {
    id: u64,
    delivery: WaiterDelivery,
}

enum WaiterDelivery {
    Blocking(Sender<Result<RelayInformationSnapshot, RelayInformationError>>),
    Async(oneshot::Sender<Result<RelayInformationSnapshot, RelayInformationError>>),
    Callback(
        Box<dyn FnOnce(Result<RelayInformationSnapshot, RelayInformationError>) + Send + 'static>,
    ),
}

impl Waiter {
    fn deliver(self, value: Result<RelayInformationSnapshot, RelayInformationError>) {
        match self.delivery {
            WaiterDelivery::Blocking(sender) => {
                let _ = sender.send(value);
            }
            WaiterDelivery::Async(sender) => {
                let _ = sender.send(value);
            }
            WaiterDelivery::Callback(callback) => callback(value),
        }
    }
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
    fn fetch(
        &self,
        relay: &RelayUrl,
        validators: Option<(&str, &str)>,
    ) -> Result<FetchResult, RelayInformationError>;

    fn fetch_cancellable(
        &self,
        relay: &RelayUrl,
        validators: Option<(&str, &str)>,
        cancellation: FetchCancellation,
    ) -> Result<FetchResult, RelayInformationError> {
        // Deterministic test fetchers are released by their test harness.
        // The production HTTP implementation overrides this method so
        // executor shutdown interrupts DNS, connect, headers, and body.
        drop(cancellation);
        self.fetch(relay, validators)
    }
}

struct HttpFetcher {
    resolver_config: Option<hickory_resolver::config::ResolverConfig>,
    resolver_strategy: hickory_resolver::config::LookupIpStrategy,
}

impl HttpFetcher {
    fn new() -> Self {
        Self {
            resolver_config: None,
            resolver_strategy: hickory_resolver::config::LookupIpStrategy::Ipv4AndIpv6,
        }
    }

    #[cfg(test)]
    fn with_resolver_config(config: hickory_resolver::config::ResolverConfig) -> Self {
        Self {
            resolver_config: Some(config),
            resolver_strategy: hickory_resolver::config::LookupIpStrategy::Ipv4Only,
        }
    }
}

impl Fetcher for HttpFetcher {
    fn fetch(
        &self,
        relay: &RelayUrl,
        validators: Option<(&str, &str)>,
    ) -> Result<FetchResult, RelayInformationError> {
        let (_cancel, receiver) = oneshot::channel();
        self.fetch_cancellable(relay, validators, FetchCancellation { receiver })
    }

    fn fetch_cancellable(
        &self,
        relay: &RelayUrl,
        validators: Option<(&str, &str)>,
        cancellation: FetchCancellation,
    ) -> Result<FetchResult, RelayInformationError> {
        let url = relay_http_url(relay);
        let validators =
            validators.map(|(etag, last_modified)| (etag.to_string(), last_modified.to_string()));
        let runtime = http_runtime()?;
        runtime.block_on(async move {
            let request = fetch_http(
                url,
                validators,
                self.resolver_config.clone(),
                self.resolver_strategy,
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

fn http_runtime() -> Result<tokio::runtime::Runtime, RelayInformationError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|error| RelayInformationError::ThreadUnavailable {
            reason: format!("HTTP runtime: {error}"),
        })
}

async fn fetch_http(
    url: String,
    validators: Option<(String, String)>,
    resolver_config: Option<hickory_resolver::config::ResolverConfig>,
    resolver_strategy: hickory_resolver::config::LookupIpStrategy,
) -> Result<FetchResult, RelayInformationError> {
    // The client is deliberately born and dropped inside this flight's
    // current-thread runtime. Hickory therefore cannot retain runtime-bound
    // DNS work, and no client clone can outlive the owned executor task.
    let resolver = HickoryReqwestResolver::new(resolver_config, resolver_strategy)?;
    let client = reqwest::Client::builder()
        .hickory_dns(true)
        .dns_resolver(Arc::new(resolver))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .referer(false)
        .timeout(FETCH_DEADLINE)
        .build()
        .map_err(|error| RelayInformationError::Http {
            reason: format!("HTTP client construction failed: {error}"),
        })?;
    let mut request = client
        .get(url)
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
}

impl HickoryReqwestResolver {
    fn new(
        config: Option<hickory_resolver::config::ResolverConfig>,
        strategy: hickory_resolver::config::LookupIpStrategy,
    ) -> Result<Self, RelayInformationError> {
        let mut builder = match config {
            Some(config) => hickory_resolver::TokioResolver::builder_with_config(
                config,
                hickory_resolver::name_server::TokioConnectionProvider::default(),
            ),
            None => hickory_resolver::TokioResolver::builder_tokio().map_err(|error| {
                RelayInformationError::Http {
                    reason: format!("could not read the system DNS configuration: {error}"),
                }
            })?,
        };
        builder.options_mut().ip_strategy = strategy;
        Ok(Self {
            resolver: builder.build(),
        })
    }
}

impl reqwest::dns::Resolve for HickoryReqwestResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let resolver = self.resolver.clone();
        let name = name.as_str().to_string();
        Box::pin(async move {
            let lookup = resolver.lookup_ip(name).await?;
            let addrs: reqwest::dns::Addrs = Box::new(
                lookup
                    .into_iter()
                    .map(|address| std::net::SocketAddr::new(address, 0)),
            );
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

fn relay_http_url(relay: &RelayUrl) -> String {
    let relay = relay.as_str();
    if let Some(rest) = relay.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = relay.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        relay.to_owned()
    }
}

impl RelayInformationService {
    pub fn new(executor: nmp_executor::Executor) -> Self {
        Self::with_executor_and_limits(
            executor,
            Arc::new(HttpFetcher::new()),
            CACHE_CAPACITY,
            WAITER_CAPACITY,
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
        let executor = nmp_executor::Executor::new(nmp_executor::DEFAULT_MAX_TASKS)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(Self::with_executor_and_limits(
            executor,
            fetcher,
            cache_capacity,
            WAITER_CAPACITY,
        ))
    }

    fn with_executor_and_limits(
        executor: nmp_executor::Executor,
        fetcher: Arc<dyn Fetcher>,
        cache_capacity: usize,
        waiter_capacity: usize,
    ) -> Self {
        assert!(cache_capacity > 0, "NIP-11 cache capacity must be non-zero");
        assert!(
            waiter_capacity > 0,
            "NIP-11 waiter capacity must be non-zero"
        );
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                closed: false,
                entries: HashMap::new(),
            }),
            access_clock: AtomicU64::new(0),
            next_flight: AtomicU64::new(1),
            next_waiter: AtomicU64::new(1),
            cache_capacity,
            waiter_capacity,
        });
        Self {
            shared,
            executor,
            fetcher,
        }
    }

    /// Read relay information once. Fresh cached values return immediately;
    /// every cache miss/revalidation consumes one zero-queue native-task
    /// reservation before the flight becomes observable.
    pub fn get(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
    ) -> Result<RelayInformationSnapshot, RelayInformationError> {
        let receiver = self.request(relay, policy)?;
        receiver
            .recv()
            .map_err(|_| RelayInformationError::ServiceClosed)?
    }

    /// Read relay information without blocking the caller while the bounded
    /// executor performs HTTP. Async and blocking callers join the same
    /// per-relay single flight and consume the same cache entry.
    pub async fn get_async(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
    ) -> Result<RelayInformationSnapshot, RelayInformationError> {
        let (reply, receiver) = oneshot::channel();
        let relay_for_cancel = relay.clone();
        let ticket = self.register(relay, policy, WaiterDelivery::Async(reply))?;
        AsyncWait {
            receiver,
            shared: Arc::clone(&self.shared),
            relay: relay_for_cancel,
            ticket,
            armed: true,
        }
        .await
    }

    pub(crate) fn request(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
    ) -> Result<
        Receiver<Result<RelayInformationSnapshot, RelayInformationError>>,
        RelayInformationError,
    > {
        let (reply, receiver) = bounded(1);
        self.register(relay, policy, WaiterDelivery::Blocking(reply))?;
        Ok(receiver)
    }

    pub(crate) fn request_callback(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
        callback: impl FnOnce(Result<RelayInformationSnapshot, RelayInformationError>) + Send + 'static,
    ) -> Result<(), RelayInformationError> {
        self.register(relay, policy, WaiterDelivery::Callback(Box::new(callback)))?;
        Ok(())
    }

    fn register(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
        delivery: WaiterDelivery,
    ) -> Result<Option<(u64, u64)>, RelayInformationError> {
        let waiter_id = self.shared.next_waiter.fetch_add(1, Ordering::Relaxed);
        let mut waiter = Some(Waiter {
            id: waiter_id,
            delivery,
        });

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
                    let mut snapshot = cached.snapshot.clone();
                    snapshot.freshness = RelayInformationFreshness::Fresh;
                    snapshot.last_error = None;
                    drop(state);
                    waiter
                        .take()
                        .expect("waiter is present")
                        .deliver(Ok(snapshot));
                    return Ok(None);
                }
            }
        }
        if let Some(flight) = entry.flight.as_mut() {
            if flight.waiters.len() >= self.shared.waiter_capacity {
                return Err(RelayInformationError::WaiterSaturated {
                    capacity: self.shared.waiter_capacity,
                });
            }
            let generation = flight.generation;
            flight
                .waiters
                .push(waiter.take().expect("waiter is present"));
            return Ok(Some((generation, waiter_id)));
        }
        if entry.cached.is_none() {
            state.entries.remove(&relay);
        }
        drop(state);

        // Reservation precedes publication: a zero-queue refusal cannot
        // create a flight or consume caller intent.
        let reservation = self
            .executor
            .reserve("NIP-11 acquisition")
            .map_err(|error| RelayInformationError::ExecutorSaturated {
                capacity: error.capacity,
            })?;

        // Another caller may have won the flight while this caller reserved.
        // Re-check every condition before publishing this generation.
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
                    let mut snapshot = cached.snapshot.clone();
                    snapshot.freshness = RelayInformationFreshness::Fresh;
                    snapshot.last_error = None;
                    drop(state);
                    drop(reservation);
                    waiter
                        .take()
                        .expect("waiter is present")
                        .deliver(Ok(snapshot));
                    return Ok(None);
                }
            }
        }
        if let Some(flight) = entry.flight.as_mut() {
            if flight.waiters.len() >= self.shared.waiter_capacity {
                return Err(RelayInformationError::WaiterSaturated {
                    capacity: self.shared.waiter_capacity,
                });
            }
            let generation = flight.generation;
            flight
                .waiters
                .push(waiter.take().expect("waiter is present"));
            drop(state);
            drop(reservation);
            return Ok(Some((generation, waiter_id)));
        }

        let generation = self.shared.next_flight.fetch_add(1, Ordering::Relaxed);
        let (cancel_sender, cancel_receiver) = oneshot::channel();
        let cancellation = Arc::new(CancelSignal {
            sender: Mutex::new(Some(cancel_sender)),
        });
        entry.flight = Some(Flight {
            generation,
            waiters: vec![waiter.take().expect("waiter is present")],
            cancellation: Arc::clone(&cancellation),
        });

        let cancel_for_executor = Arc::clone(&cancellation);
        let starter = match reservation.start_with_cancel(move || {
            cancel_for_executor.cancel();
        }) {
            Ok(starter) => starter,
            Err(error) => {
                if entry
                    .flight
                    .as_ref()
                    .is_some_and(|flight| flight.generation == generation)
                {
                    entry.flight = None;
                }
                if entry.cached.is_none() {
                    state.entries.remove(&relay);
                }
                return Err(RelayInformationError::ThreadUnavailable {
                    reason: error.to_string(),
                });
            }
        };
        drop(state);

        let shared = Arc::clone(&self.shared);
        let fetcher = Arc::clone(&self.fetcher);
        let task_relay = relay.clone();
        starter.run(move || {
            worker(
                shared,
                task_relay,
                generation,
                fetcher,
                FetchCancellation {
                    receiver: cancel_receiver,
                },
            );
        });
        Ok(Some((generation, waiter_id)))
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
        let mut snapshot = cached.snapshot.clone();
        snapshot.freshness = if now_secs() < cached.fresh_until {
            RelayInformationFreshness::Fresh
        } else {
            RelayInformationFreshness::Stale
        };
        Some(snapshot)
    }

    /// Refuse new acquisition and resolve every admitted waiter. Running
    /// fetches are signalled independently; their exact-generation late
    /// completion is ignored.
    pub(crate) fn close(&self) {
        let (waiters, cancellations) = {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if state.closed {
                return;
            }
            state.closed = true;
            let mut waiters = Vec::new();
            let mut cancellations = Vec::new();
            for entry in state.entries.values_mut() {
                if let Some(flight) = entry.flight.take() {
                    waiters.extend(flight.waiters);
                    cancellations.push(flight.cancellation);
                }
            }
            state.entries.retain(|_, entry| entry.cached.is_some());
            (waiters, cancellations)
        };
        for cancellation in cancellations {
            cancellation.cancel();
        }
        for waiter in waiters {
            waiter.deliver(Err(RelayInformationError::ServiceClosed));
        }
    }
}

fn worker(
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
        .and_then(|value| value.snapshot.etag.as_deref())
        .unwrap_or("");
    let last_modified = cached
        .as_ref()
        .and_then(|value| value.snapshot.last_modified.as_deref())
        .unwrap_or("");
    let validators =
        (!etag.is_empty() || !last_modified.is_empty()).then_some((etag, last_modified));
    let result = fetcher
        .fetch_cancellable(&relay, validators, cancellation)
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
        let fresh_for = fetched.fresh_for.unwrap_or(DEFAULT_FRESH_FOR);
        Ok(RelayInformationSnapshot {
            relay: relay.clone(),
            document,
            document_revision: blake3::hash(raw_json.as_bytes()).to_hex().to_string(),
            raw_json,
            fetched_at: now_secs(),
            fresh_until: now_secs().saturating_add(fresh_for.as_secs()),
            freshness: RelayInformationFreshness::Fresh,
            etag: fetched.etag,
            last_modified: fetched.last_modified,
            cache_control: fetched.cache_control,
            expires: fetched.expires,
            last_error: None,
        })
    } else {
        let cached = cached.ok_or_else(|| RelayInformationError::Http {
            reason: "relay returned 304 without a cached document".to_string(),
        })?;
        let mut snapshot = cached.snapshot.clone();
        snapshot.cache_control = fetched.cache_control.or(snapshot.cache_control);
        snapshot.expires = fetched.expires.or(snapshot.expires);
        let fresh_for = fetched
            .fresh_for
            .or_else(|| {
                fresh_for_headers(
                    snapshot.cache_control.as_deref(),
                    snapshot.expires.as_deref(),
                    SystemTime::now(),
                )
            })
            .unwrap_or(DEFAULT_FRESH_FOR);
        snapshot.fetched_at = now_secs();
        snapshot.fresh_until = snapshot.fetched_at.saturating_add(fresh_for.as_secs());
        snapshot.freshness = RelayInformationFreshness::Fresh;
        snapshot.etag = fetched.etag.or(snapshot.etag);
        snapshot.last_modified = fetched.last_modified.or(snapshot.last_modified);
        snapshot.last_error = None;
        Ok(snapshot)
    }
}

struct AsyncWait {
    receiver: oneshot::Receiver<Result<RelayInformationSnapshot, RelayInformationError>>,
    shared: Arc<Shared>,
    relay: RelayUrl,
    ticket: Option<(u64, u64)>,
    armed: bool,
}

impl Future for AsyncWait {
    type Output = Result<RelayInformationSnapshot, RelayInformationError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.receiver).poll(cx) {
            Poll::Ready(Ok(value)) => {
                self.armed = false;
                Poll::Ready(value)
            }
            Poll::Ready(Err(_)) => {
                self.armed = false;
                Poll::Ready(Err(RelayInformationError::ServiceClosed))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for AsyncWait {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some((generation, waiter_id)) = self.ticket {
            cancel_waiter(&self.shared, &self.relay, generation, waiter_id);
        }
    }
}

fn cancel_waiter(shared: &Shared, relay: &RelayUrl, generation: u64, waiter_id: u64) {
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
        flight.waiters.retain(|waiter| waiter.id != waiter_id);
        if !flight.waiters.is_empty() {
            return;
        }
        let cancellation = entry
            .flight
            .take()
            .expect("the exact empty flight is present")
            .cancellation;
        if entry.cached.is_none() {
            state.entries.remove(relay);
        }
        cancellation
    };
    cancellation.cancel();
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

        let waiters = state
            .entries
            .get_mut(relay)
            .and_then(|entry| entry.flight.take())
            .expect("the exact flight is present")
            .waiters;
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
                        fresh_until: snapshot.fresh_until,
                    });
                }
                Ok(snapshot)
            }
            Err(error) => {
                let allows_stale = !matches!(
                    error,
                    RelayInformationError::ExecutorSaturated { .. }
                        | RelayInformationError::WaiterSaturated { .. }
                        | RelayInformationError::ThreadUnavailable { .. }
                        | RelayInformationError::ServiceClosed
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
                        cached.snapshot.fresh_until = stale_at;
                        cached.snapshot.freshness = RelayInformationFreshness::Stale;
                        cached.snapshot.last_error = Some(error.clone());
                        Ok(cached.snapshot.clone())
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
        Some((waiters, delivered))
    };
    let Some((waiters, delivered)) = completion else {
        return;
    };
    for waiter in waiters {
        waiter.deliver(delivered.clone());
    }
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
    use std::time::Instant;

    use super::*;

    struct CountingFetcher {
        calls: AtomicUsize,
        fail_after_first: bool,
    }

    struct GatedFetcher {
        started: Sender<()>,
        release: Receiver<()>,
    }

    struct MalformedThenGoodFetcher {
        calls: AtomicUsize,
    }

    impl Fetcher for GatedFetcher {
        fn fetch(
            &self,
            _relay: &RelayUrl,
            _validators: Option<(&str, &str)>,
        ) -> Result<FetchResult, RelayInformationError> {
            let _ = self.started.send(());
            self.release
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
        fn fetch(
            &self,
            _relay: &RelayUrl,
            _validators: Option<(&str, &str)>,
        ) -> Result<FetchResult, RelayInformationError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_after_first && call > 0 {
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
        }
    }

    impl Fetcher for MalformedThenGoodFetcher {
        fn fetch(
            &self,
            _relay: &RelayUrl,
            _validators: Option<(&str, &str)>,
        ) -> Result<FetchResult, RelayInformationError> {
            let raw_json = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                "not-json"
            } else {
                r#"{"name":"Recovered"}"#
            };
            Ok(FetchResult {
                raw_json: Some(raw_json.to_string()),
                etag: None,
                last_modified: None,
                cache_control: None,
                expires: None,
                fresh_for: Some(DEFAULT_FRESH_FOR),
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
        let a = service
            .request(relay.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        started_rx.recv().unwrap();
        let b = service
            .request(canonical_equivalent, RelayInformationCachePolicy::Refresh)
            .unwrap();
        release_tx.send(()).unwrap();
        let a = a.recv().unwrap().unwrap();
        let b = b.recv().unwrap().unwrap();
        assert_eq!(a, b);
        assert_eq!(a.document.name.as_deref(), Some("Async"));
        assert_eq!(a.document_revision.len(), 64);
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
        assert_eq!(snapshot.document.name.as_deref(), Some("Async"));
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
        assert_eq!(recovered.document.name.as_deref(), Some("Recovered"));
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
        service
            .get(relay.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        let stale = service
            .get(relay.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        assert_eq!(stale.document.name.as_deref(), Some("Example"));
        assert_eq!(stale.freshness, RelayInformationFreshness::Stale);
        assert!(matches!(
            stale.last_error,
            Some(RelayInformationError::Http { .. })
        ));

        let still_stale = service
            .get(relay, RelayInformationCachePolicy::UseCache)
            .unwrap();
        assert_eq!(still_stale.freshness, RelayInformationFreshness::Stale);
        assert!(matches!(
            still_stale.last_error,
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
        let error = match HttpFetcher::new().fetch(&relay, None) {
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
        let executor = nmp_executor::Executor::new(2).unwrap();
        let service = RelayInformationService::new(executor.clone());
        let value = service
            .get(relay.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        server.join().unwrap();

        assert_eq!(value.cache_control.as_deref(), Some("no-cache"));
        assert!(value.fresh_until <= value.fetched_at);
        assert_eq!(
            service.cached(&relay).unwrap().freshness,
            RelayInformationFreshness::Stale
        );
        service.close();
        executor.shutdown();
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
        let executor = nmp_executor::Executor::new(2).unwrap();
        let service = RelayInformationService::new(executor.clone());
        let value = service
            .get(relay, RelayInformationCachePolicy::Refresh)
            .unwrap();
        server.join().unwrap();

        assert_eq!(value.expires.as_deref(), Some(expected_expires.as_str()));
        assert!(value.fresh_until.saturating_sub(value.fetched_at) >= 115);
        assert!(value.fresh_until.saturating_sub(value.fetched_at) <= 120);
        service.close();
        executor.shutdown();
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
        let executor = nmp_executor::Executor::new(2).unwrap();
        let service = RelayInformationService::new(executor.clone());

        let maximum = service
            .get(relay.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        assert_eq!(maximum.fresh_until, u64::MAX);
        let retried = service
            .get(relay, RelayInformationCachePolicy::Refresh)
            .unwrap();
        assert_eq!(retried.document.name.as_deref(), Some("Retried"));

        server.join().unwrap();
        service.close();
        executor.shutdown();
    }

    #[test]
    fn refreshing_cache_entries_count_toward_the_bound_and_257th_is_not_retained() {
        let mut entries = HashMap::new();
        for index in 0..CACHE_CAPACITY {
            let relay = RelayUrl::parse(&format!("wss://cached-{index}.example")).unwrap();
            let snapshot = finish_fetch(
                &relay,
                None,
                FetchResult {
                    raw_json: Some(format!(r#"{{"name":"cached-{index}"}}"#)),
                    etag: None,
                    last_modified: None,
                    cache_control: None,
                    expires: None,
                    fresh_for: Some(DEFAULT_FRESH_FOR),
                },
            )
            .unwrap();
            let (cancel, _cancelled) = oneshot::channel();
            entries.insert(
                relay,
                Entry {
                    cached: Some(Cached {
                        fresh_until: snapshot.fresh_until,
                        snapshot,
                    }),
                    flight: Some(Flight {
                        generation: index as u64 + 1,
                        waiters: Vec::new(),
                        cancellation: Arc::new(CancelSignal {
                            sender: Mutex::new(Some(cancel)),
                        }),
                    }),
                    last_access: index as u64,
                },
            );
        }
        let relay_257 = RelayUrl::parse("wss://uncached-257.example").unwrap();
        let generation_257 = 10_000;
        let (cancel, _cancelled) = oneshot::channel();
        entries.insert(
            relay_257.clone(),
            Entry {
                cached: None,
                flight: Some(Flight {
                    generation: generation_257,
                    waiters: Vec::new(),
                    cancellation: Arc::new(CancelSignal {
                        sender: Mutex::new(Some(cancel)),
                    }),
                }),
                last_access: u64::MAX,
            },
        );
        let shared = Shared {
            state: Mutex::new(State {
                closed: false,
                entries,
            }),
            access_clock: AtomicU64::new(0),
            next_flight: AtomicU64::new(20_000),
            next_waiter: AtomicU64::new(1),
            cache_capacity: CACHE_CAPACITY,
            waiter_capacity: WAITER_CAPACITY,
        };
        let completed = finish_fetch(
            &relay_257,
            None,
            FetchResult {
                raw_json: Some(r#"{"name":"fresh-but-not-retained"}"#.to_string()),
                etag: None,
                last_modified: None,
                cache_control: None,
                expires: None,
                fresh_for: Some(DEFAULT_FRESH_FOR),
            },
        )
        .unwrap();

        complete(&shared, &relay_257, generation_257, Ok(completed));

        let state = shared.state.lock().unwrap();
        assert_eq!(
            state
                .entries
                .values()
                .filter(|entry| entry.cached.is_some())
                .count(),
            CACHE_CAPACITY
        );
        assert!(!state.entries.contains_key(&relay_257));
        assert!(state
            .entries
            .values()
            .all(|entry| { entry.cached.is_some() && entry.flight.is_some() }));
    }

    #[test]
    fn waiter_saturation_is_typed_and_close_resolves_every_admitted_waiter() {
        let (started_tx, started_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let executor = nmp_executor::Executor::new(2).unwrap();
        let service = RelayInformationService::with_executor_and_limits(
            executor.clone(),
            Arc::new(GatedFetcher {
                started: started_tx,
                release: release_rx,
            }),
            2,
            2,
        );
        let relay = RelayUrl::parse("wss://saturated.example").unwrap();
        let first = service
            .request(relay.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        started_rx.recv().unwrap();
        let second = service
            .request(relay.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        assert!(matches!(
            service.request(relay, RelayInformationCachePolicy::Refresh),
            Err(RelayInformationError::WaiterSaturated { capacity: 2 })
        ));

        service.close();
        assert_eq!(
            first.recv().unwrap(),
            Err(RelayInformationError::ServiceClosed)
        );
        assert_eq!(
            second.recv().unwrap(),
            Err(RelayInformationError::ServiceClosed)
        );
        release_tx.send(()).unwrap();
        executor.shutdown();
    }

    #[test]
    fn executor_saturation_refuses_without_publishing_a_flight() {
        let executor = nmp_executor::Executor::new(1).unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let cancel = release_tx.clone();
        executor
            .spawn_with_cancel(
                "held obligation",
                move || {
                    let _ = cancel.send(());
                },
                move || {
                    let _ = release_rx.recv();
                },
            )
            .unwrap();
        let service = RelayInformationService::new(executor.clone());
        let relay = RelayUrl::parse("wss://refused.example").unwrap();
        assert!(matches!(
            service.request(relay, RelayInformationCachePolicy::Refresh),
            Err(RelayInformationError::ExecutorSaturated { capacity: 1 })
        ));
        assert!(service.shared.state.lock().unwrap().entries.is_empty());
        assert_eq!(executor.census().admitted, 1);
        assert_eq!(executor.census().running, 1);
        service.close();
        release_tx.send(()).unwrap();
        executor.shutdown();
    }

    #[test]
    fn dropping_the_last_async_waiter_cancels_its_exact_generation() {
        let (started_tx, started_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let executor = nmp_executor::Executor::new(1).unwrap();
        let service = RelayInformationService::with_executor_and_limits(
            executor.clone(),
            Arc::new(GatedFetcher {
                started: started_tx,
                release: release_rx,
            }),
            2,
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
        release_tx.send(()).unwrap();
        executor.shutdown();
    }

    #[test]
    fn late_old_generation_cannot_overwrite_or_drain_the_new_flight() {
        let relay = RelayUrl::parse("wss://generation.example").unwrap();
        let (reply, receiver) = bounded(1);
        let (cancel, _cancelled) = oneshot::channel();
        let mut entries = HashMap::new();
        entries.insert(
            relay.clone(),
            Entry {
                cached: None,
                flight: Some(Flight {
                    generation: 2,
                    waiters: vec![Waiter {
                        id: 2,
                        delivery: WaiterDelivery::Blocking(reply),
                    }],
                    cancellation: Arc::new(CancelSignal {
                        sender: Mutex::new(Some(cancel)),
                    }),
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
            next_waiter: AtomicU64::new(3),
            cache_capacity: 2,
            waiter_capacity: 2,
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
        complete(&shared, &relay, 1, Ok(old));
        assert!(matches!(
            receiver.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ));
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
            receiver.recv().unwrap().unwrap().document.name.as_deref(),
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
        let executor = nmp_executor::Executor::new(2).unwrap();
        let service = RelayInformationService::new(executor.clone());
        let retained = service.clone();
        let relay = RelayUrl::parse(&format!("ws://{address}")).unwrap();
        let receiver = service
            .request(relay.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        accepted.wait();

        let started = Instant::now();
        service.close();
        executor.shutdown();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            receiver.recv().unwrap(),
            Err(RelayInformationError::ServiceClosed)
        );
        assert!(matches!(
            retained.request(relay, RelayInformationCachePolicy::Refresh),
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
        let error = HttpFetcher::new().fetch(&relay, None).unwrap_err();
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
            HttpFetcher::new().fetch(&relay, None),
            Err(RelayInformationError::Http { .. })
        ));
        server.join().unwrap();
    }

    #[test]
    fn hickory_resolves_a_dns_hostname_without_system_gai() {
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
        let nameservers = hickory_resolver::config::NameServerConfigGroup::from_ips_clear(
            &[dns_address.ip()],
            dns_address.port(),
            true,
        );
        let resolver =
            hickory_resolver::config::ResolverConfig::from_parts(None, Vec::new(), nameservers);
        let relay = RelayUrl::parse(&format!("ws://relay.nmp.test:{port}")).unwrap();
        let value = HttpFetcher::with_resolver_config(resolver)
            .fetch(&relay, None)
            .unwrap();
        assert!(value.raw_json.is_some_and(|json| json.contains("Hostname")));
        dns_server.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn http_runtime_has_one_current_thread_worker_and_no_tokio_worker_pool() {
        let runtime = http_runtime().unwrap();
        assert_eq!(
            runtime.handle().runtime_flavor(),
            tokio::runtime::RuntimeFlavor::CurrentThread
        );
        assert_eq!(runtime.metrics().num_workers(), 1);
    }

    #[test]
    fn websocket_urls_map_to_http_without_losing_path() {
        assert_eq!(
            relay_http_url(&RelayUrl::parse("wss://relay.example/nostr").unwrap()),
            "https://relay.example/nostr"
        );
    }
}
