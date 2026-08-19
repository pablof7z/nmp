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
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_channel::oneshot;
use nostr::RelayUrl;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

use crate::value::{
    RelayInformationCachePolicy, RelayInformationDocument, RelayInformationError,
    RelayInformationFreshness, RelayInformationLimitations, RelayInformationSnapshot,
};

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
/// Caller-held snapshots share the cached payload. They are outside this
/// census; only service-owned cache and flight state are counted.
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

enum State {
    Open { entries: HashMap<RelayUrl, Entry> },
    Closed,
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

struct HttpFetcher;

/// An HTTP URL whose authority has been proven not to contain userinfo.
/// Keeping this distinct from `String` makes the no-Authorization invariant
/// a prerequisite of `fetch_http`, not a request-builder convention.
struct UncredentialedHttpUrl(reqwest::Url);

impl HttpFetcher {
    fn new() -> Self {
        Self
    }
}

impl HttpFetcher {
}

impl Fetcher for HttpFetcher {
    fn fetch_cancellable_async<'a>(
        &'a self,
        relay: RelayUrl,
        validators: Option<(String, String)>,
        cancellation: FetchCancellation,
    ) -> Pin<Box<dyn Future<Output = Result<FetchResult, RelayInformationError>> + Send + 'a>> {
        Box::pin(async move {
            let url = relay_http_url(&relay)?;
            let request = fetch_http(url, validators);
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

async fn fetch_http(
    url: UncredentialedHttpUrl,
    validators: Option<(String, String)>,
) -> Result<FetchResult, RelayInformationError> {
    // Use reqwest's normal platform resolver. NMP owns no DNS implementation,
    // answer-set filtering, address pinning, or address-class policy.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .referer(false)
        .timeout(FETCH_DEADLINE)
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

    fn with_runtime_and_limits(
        runtime: tokio::runtime::Handle,
        fetcher: Arc<dyn Fetcher>,
        cache_capacity: usize,
    ) -> Self {
        assert!(cache_capacity > 0, "NIP-11 cache capacity must be non-zero");
        let shared = Arc::new(Shared {
            state: Mutex::new(State::Open {
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

    /// Fire-and-forget acquisition: the engine's connect path asks for a
    /// document and takes the answer on whichever worker finishes it, without
    /// a caller thread to block.
    pub fn request_callback(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
        callback: impl FnOnce(Result<RelayInformationSnapshot, RelayInformationError>) + Send + 'static,
    ) -> Result<(), RelayInformationError> {
        match &*self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
        {
            State::Open { .. } => {}
            State::Closed => return Err(RelayInformationError::ServiceClosed),
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
        let entries = match &mut *state {
            State::Open { entries } => entries,
            State::Closed => return Err(RelayInformationError::ServiceClosed),
        };
        let access = self.shared.access_clock.fetch_add(1, Ordering::Relaxed);
        let entry = entries.entry(relay.clone()).or_default();
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
        let entries = match &mut *state {
            State::Open { entries } => entries,
            State::Closed => return None,
        };
        let access = self.shared.access_clock.fetch_add(1, Ordering::Relaxed);
        let entry = entries.get_mut(relay)?;
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
    pub fn retention_census(&self) -> RelayInformationRetentionCensus {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let entries = match &*state {
            State::Open { entries } => entries,
            State::Closed => {
                return RelayInformationRetentionCensus {
                    cached_entries: 0,
                    cached_payloads: 0,
                    cached_raw_body_bytes: 0,
                    active_flights: 0,
                    subscribed_callers: 0,
                    max_active_flights: MAX_ACTIVE_FETCHES,
                };
            }
        };
        let mut payloads = std::collections::HashSet::new();
        let mut cached_raw_body_bytes = 0usize;
        let mut cached_entries = 0usize;
        let mut active_flights = 0usize;
        let mut subscribed_callers = 0usize;
        for entry in entries.values() {
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
    pub fn close(&self) {
        self.shared.fetch_slots.close();
        let entries = {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match std::mem::replace(&mut *state, State::Closed) {
                State::Open { entries } => entries,
                State::Closed => return,
            }
        };
        let flights = entries
            .into_values()
            .filter_map(|entry| entry.flight)
            .collect::<Vec<_>>();
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
        let entries = match &*state {
            State::Open { entries } => entries,
            State::Closed => return,
        };
        let Some(entry) = entries.get(&relay) else {
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
        let entries = match &mut *state {
            State::Open { entries } => entries,
            State::Closed => return,
        };
        let Some(entry) = entries.get_mut(relay) else {
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
            entries.remove(relay);
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
        let entries = match &mut *state {
            State::Open { entries } => entries,
            State::Closed => return,
        };
        let Some(entry) = entries.get(relay) else {
            return;
        };
        if !entry
            .flight
            .as_ref()
            .is_some_and(|flight| flight.generation == generation)
        {
            return;
        }

        let flight = entries
            .get_mut(relay)
            .and_then(|entry| entry.flight.take())
            .expect("the exact flight is present");
        let access = shared.access_clock.fetch_add(1, Ordering::Relaxed);
        let delivered = match result {
            Ok(snapshot) => {
                let needs_slot = entries
                    .get(relay)
                    .is_none_or(|entry| entry.cached.is_none());
                let mut retain_snapshot = true;
                if needs_slot
                    && entries
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
                    let eviction = entries
                        .iter()
                        .filter(|(candidate, entry)| {
                            *candidate != relay && entry.cached.is_some() && entry.flight.is_none()
                        })
                        .min_by_key(|(_, entry)| entry.last_access)
                        .map(|(candidate, _)| candidate.clone());
                    if let Some(eviction) = eviction {
                        entries.remove(&eviction);
                    } else {
                        retain_snapshot = false;
                    }
                }
                if retain_snapshot {
                    let entry = entries.entry(relay.clone()).or_default();
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
                match entries.entry(relay.clone()).or_default().cached.as_mut() {
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
        entries.retain(|_, entry| entry.cached.is_some() || entry.flight.is_some());
        debug_assert!(
            entries
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

