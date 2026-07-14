//! Engine-owned, one-shot NIP-11 acquisition.
//!
//! NIP-11 is HTTP state, not a reactive stream. This service gives callers
//! an explicit one-shot read while sharing a bounded, in-memory cache and a
//! per-relay single flight. The last good document is retained separately
//! from the last acquisition error, so a transient failure never destroys
//! useful presentation or capability evidence.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use futures_channel::oneshot;
use nostr::RelayUrl;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_FRESH_FOR: Duration = Duration::from_secs(60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
const WORKER_COUNT: usize = 8;
const QUEUE_CAPACITY: usize = 64;
const CACHE_CAPACITY: usize = 256;

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
    QueueFull,
    ServiceClosed,
    Http { reason: String },
    ResponseTooLarge { limit_bytes: u64 },
    InvalidDocument { reason: String },
}

impl std::fmt::Display for RelayInformationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull => f.write_str("NIP-11 acquisition queue is full"),
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
    workers: Arc<WorkerPool>,
}

struct WorkerPool {
    jobs: Mutex<Option<Sender<RelayUrl>>>,
    joins: Mutex<Vec<JoinHandle<()>>>,
}

impl WorkerPool {
    fn sender(&self) -> Option<Sender<RelayUrl>> {
        self.jobs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        // Close the queue before joining. Workers own only receiver clones,
        // so dropping the final sender wakes every idle `recv` without a
        // polling interval or an out-of-band leaked thread.
        self.jobs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        for join in self
            .joins
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .drain(..)
        {
            let _ = join.join();
        }
    }
}

struct Shared {
    entries: Mutex<HashMap<RelayUrl, Entry>>,
    access_clock: AtomicU64,
    cache_capacity: usize,
}

#[derive(Default)]
struct Entry {
    cached: Option<Cached>,
    in_flight: Vec<Waiter>,
    last_access: u64,
}

enum Waiter {
    Blocking(Sender<Result<RelayInformationSnapshot, RelayInformationError>>),
    Async(oneshot::Sender<Result<RelayInformationSnapshot, RelayInformationError>>),
}

impl Waiter {
    fn deliver(self, value: Result<RelayInformationSnapshot, RelayInformationError>) {
        match self {
            Self::Blocking(sender) => {
                let _ = sender.send(value);
            }
            Self::Async(sender) => {
                let _ = sender.send(value);
            }
        }
    }
}

#[derive(Clone)]
struct Cached {
    snapshot: RelayInformationSnapshot,
    fresh_until: SystemTime,
}

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
}

struct HttpFetcher {
    agent: ureq::Agent,
}

impl HttpFetcher {
    fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(FETCH_TIMEOUT)
            .timeout_read(FETCH_TIMEOUT)
            .timeout_write(FETCH_TIMEOUT)
            // Redirects are deliberately refused. Following even one hop
            // would let a public relay URL redirect this host-process HTTP
            // client to loopback, RFC-1918, link-local, or another local
            // service after the original relay URL had passed admission.
            // With zero automatic redirects, no redirect target is ever
            // resolved or contacted and request headers never cross origins.
            .redirects(0)
            .build();
        Self { agent }
    }
}

impl Fetcher for HttpFetcher {
    fn fetch(
        &self,
        relay: &RelayUrl,
        validators: Option<(&str, &str)>,
    ) -> Result<FetchResult, RelayInformationError> {
        let url = relay_http_url(relay);
        let mut request = self.agent.get(&url).set("Accept", "application/nostr+json");
        if let Some((etag, last_modified)) = validators {
            if !etag.is_empty() {
                request = request.set("If-None-Match", etag);
            }
            if !last_modified.is_empty() {
                request = request.set("If-Modified-Since", last_modified);
            }
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(304, response)) => response,
            Err(error) => {
                return Err(RelayInformationError::Http {
                    reason: error.to_string(),
                })
            }
        };
        if (300..400).contains(&response.status()) && response.status() != 304 {
            return Err(RelayInformationError::Http {
                reason: "NIP-11 redirects are not followed".to_string(),
            });
        }
        let cache_control = response.header("Cache-Control").map(str::to_owned);
        let expires = response.header("Expires").map(str::to_owned);
        let fresh_for = response_fresh_for(&response);
        let etag = response.header("ETag").map(str::to_owned);
        let last_modified = response.header("Last-Modified").map(str::to_owned);
        if response.status() == 304 {
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
        response
            .into_reader()
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| RelayInformationError::Http {
                reason: error.to_string(),
            })?;
        if bytes.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(RelayInformationError::ResponseTooLarge {
                limit_bytes: MAX_RESPONSE_BYTES,
            });
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
}

fn response_fresh_for(response: &ureq::Response) -> Option<Duration> {
    fresh_for_headers(
        response.header("Cache-Control"),
        response.header("Expires"),
        SystemTime::now(),
    )
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
    pub fn try_new() -> std::io::Result<Self> {
        Self::try_with_fetcher(Arc::new(HttpFetcher::new()))
    }

    fn try_with_fetcher(fetcher: Arc<dyn Fetcher>) -> std::io::Result<Self> {
        Self::with_fetcher_and_capacity(fetcher, CACHE_CAPACITY)
    }

    fn with_fetcher_and_capacity(
        fetcher: Arc<dyn Fetcher>,
        cache_capacity: usize,
    ) -> std::io::Result<Self> {
        assert!(cache_capacity > 0, "NIP-11 cache capacity must be non-zero");
        let shared = Arc::new(Shared {
            entries: Mutex::new(HashMap::new()),
            access_clock: AtomicU64::new(0),
            cache_capacity,
        });
        let (jobs, receiver) = bounded::<RelayUrl>(QUEUE_CAPACITY);
        let mut joins = Vec::with_capacity(WORKER_COUNT);
        for index in 0..WORKER_COUNT {
            let shared = Arc::clone(&shared);
            let receiver = receiver.clone();
            let fetcher = Arc::clone(&fetcher);
            match std::thread::Builder::new()
                .name(format!("nmp-nip11-{index}"))
                .spawn(move || worker(shared, receiver, fetcher))
            {
                Ok(join) => joins.push(join),
                Err(error) => {
                    drop(jobs);
                    for join in joins {
                        let _ = join.join();
                    }
                    return Err(error);
                }
            }
        }
        Ok(Self {
            shared,
            workers: Arc::new(WorkerPool {
                jobs: Mutex::new(Some(jobs)),
                joins: Mutex::new(joins),
            }),
        })
    }

    /// Read relay information once. Fresh cached values return immediately;
    /// every cache miss/revalidation is executed by the bounded worker pool.
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
    /// worker pool performs HTTP. Async and blocking callers join the same
    /// per-relay single flight and consume the same cache entry.
    pub async fn get_async(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
    ) -> Result<RelayInformationSnapshot, RelayInformationError> {
        let (reply, receiver) = oneshot::channel();
        self.register(relay, policy, Waiter::Async(reply))?;
        receiver
            .await
            .map_err(|_| RelayInformationError::ServiceClosed)?
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
        self.register(relay, policy, Waiter::Blocking(reply))?;
        Ok(receiver)
    }

    fn register(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
        waiter: Waiter,
    ) -> Result<(), RelayInformationError> {
        let mut entries = self
            .shared
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let access = self.shared.access_clock.fetch_add(1, Ordering::Relaxed);
        let entry = entries.entry(relay.clone()).or_default();
        entry.last_access = access;
        if policy == RelayInformationCachePolicy::UseCache {
            if let Some(cached) = &entry.cached {
                if SystemTime::now() < cached.fresh_until {
                    let mut snapshot = cached.snapshot.clone();
                    snapshot.freshness = RelayInformationFreshness::Fresh;
                    snapshot.last_error = None;
                    waiter.deliver(Ok(snapshot));
                    return Ok(());
                }
            }
        }
        let first = entry.in_flight.is_empty();
        entry.in_flight.push(waiter);
        drop(entries);
        if first {
            let Some(jobs) = self.workers.sender() else {
                complete(
                    &self.shared,
                    &relay,
                    Err(RelayInformationError::ServiceClosed),
                );
                return Ok(());
            };
            match jobs.try_send(relay.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    complete(&self.shared, &relay, Err(RelayInformationError::QueueFull));
                }
                Err(TrySendError::Disconnected(_)) => {
                    complete(
                        &self.shared,
                        &relay,
                        Err(RelayInformationError::ServiceClosed),
                    );
                }
            }
        }
        Ok(())
    }

    /// Return the current last-good value without initiating I/O.
    pub fn cached(&self, relay: &RelayUrl) -> Option<RelayInformationSnapshot> {
        let mut entries = self
            .shared
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let access = self.shared.access_clock.fetch_add(1, Ordering::Relaxed);
        let entry = entries.get_mut(relay)?;
        entry.last_access = access;
        let cached = entry.cached.as_ref()?;
        let mut snapshot = cached.snapshot.clone();
        snapshot.freshness = if SystemTime::now() < cached.fresh_until {
            RelayInformationFreshness::Fresh
        } else {
            RelayInformationFreshness::Stale
        };
        Some(snapshot)
    }
}

fn worker(shared: Arc<Shared>, jobs: Receiver<RelayUrl>, fetcher: Arc<dyn Fetcher>) {
    while let Ok(relay) = jobs.recv() {
        let cached = {
            let entries = shared
                .entries
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            entries.get(&relay).and_then(|entry| entry.cached.clone())
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
            .fetch(&relay, validators)
            .and_then(|fetched| finish_fetch(&relay, cached.as_ref(), fetched));
        complete(&shared, &relay, result);
    }
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

fn complete(
    shared: &Shared,
    relay: &RelayUrl,
    result: Result<RelayInformationSnapshot, RelayInformationError>,
) {
    let (waiters, delivered) = {
        let mut entries = shared
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let access = shared.access_clock.fetch_add(1, Ordering::Relaxed);
        let delivered = match result {
            Ok(snapshot) => {
                let needs_slot = entries
                    .get(relay)
                    .is_none_or(|entry| entry.cached.is_none());
                if needs_slot
                    && entries
                        .values()
                        .filter(|entry| entry.cached.is_some())
                        .count()
                        >= shared.cache_capacity
                {
                    let eviction = entries
                        .iter()
                        .filter(|(candidate, entry)| {
                            *candidate != relay
                                && entry.cached.is_some()
                                && entry.in_flight.is_empty()
                        })
                        .min_by_key(|(_, entry)| entry.last_access)
                        .map(|(candidate, _)| candidate.clone());
                    if let Some(eviction) = eviction {
                        entries.remove(&eviction);
                    }
                }
                let entry = entries.entry(relay.clone()).or_default();
                entry.last_access = access;
                entry.cached = Some(Cached {
                    snapshot: snapshot.clone(),
                    fresh_until: UNIX_EPOCH + Duration::from_secs(snapshot.fresh_until),
                });
                Ok(snapshot)
            }
            Err(error) => match entries.entry(relay.clone()).or_default().cached.as_mut() {
                Some(cached) => {
                    // A failed explicit refresh is new evidence that the
                    // last-good representation cannot keep using its prior
                    // freshness deadline. Expire both views of that deadline
                    // so a later `UseCache` read cannot silently relabel the
                    // stale-on-error value as fresh and erase the failure.
                    let stale_at = now_secs();
                    cached.fresh_until = UNIX_EPOCH;
                    cached.snapshot.fresh_until = stale_at;
                    cached.snapshot.freshness = RelayInformationFreshness::Stale;
                    cached.snapshot.last_error = Some(error.clone());
                    Ok(cached.snapshot.clone())
                }
                None => Err(error),
            },
        };
        let waiters = entries
            .get_mut(relay)
            .map(|entry| std::mem::take(&mut entry.in_flight))
            .unwrap_or_default();
        entries.retain(|_, entry| entry.cached.is_some() || !entry.in_flight.is_empty());
        (waiters, delivered)
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
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

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
        let fetcher = Arc::new(CountingFetcher {
            calls: AtomicUsize::new(0),
            fail_after_first: false,
        });
        let service = RelayInformationService::try_with_fetcher(fetcher.clone()).unwrap();
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let canonical_equivalent = RelayUrl::parse("wss://relay.example/").unwrap();
        assert_eq!(relay, canonical_equivalent);
        let a = service
            .request(relay.clone(), RelayInformationCachePolicy::Refresh)
            .unwrap();
        let b = service
            .request(canonical_equivalent, RelayInformationCachePolicy::Refresh)
            .unwrap();
        let a = a.recv().unwrap().unwrap();
        let b = b.recv().unwrap().unwrap();
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
        assert_eq!(a, b);
        assert_eq!(a.document.name.as_deref(), Some("Example"));
        assert_eq!(a.advertises_nip(77), Some(true));
        assert_eq!(a.document_revision.len(), 64);
        assert_eq!(a.document.limitation.max_subscriptions, Some(20));
        assert_eq!(a.document.limitation.max_limit, Some(500));
        assert_eq!(a.document.limitation.auth_required, Some(true));
        assert!(a.raw_json.contains("future"));
        assert!(a
            .document
            .structured
            .get("limitation")
            .is_some_and(|raw| raw.contains("future_limit")));
        assert_eq!(
            a.document.structured.get("future").map(String::as_str),
            Some(r#"{"x":1}"#)
        );
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
        let service = RelayInformationService::try_new().unwrap();
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
        let value = RelayInformationService::try_new()
            .unwrap()
            .get(relay, RelayInformationCachePolicy::Refresh)
            .unwrap();
        server.join().unwrap();

        assert_eq!(value.expires.as_deref(), Some(expected_expires.as_str()));
        assert!(value.fresh_until.saturating_sub(value.fetched_at) >= 115);
        assert!(value.fresh_until.saturating_sub(value.fetched_at) <= 120);
    }

    #[test]
    fn websocket_urls_map_to_http_without_losing_path() {
        assert_eq!(
            relay_http_url(&RelayUrl::parse("wss://relay.example/nostr").unwrap()),
            "https://relay.example/nostr"
        );
    }
}
