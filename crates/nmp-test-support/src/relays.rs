//! `ScriptedRelay` — the in-process, real-websocket relay every scenario's
//! world topology is built from (approach doc §2.2/§2.3). Wraps
//! `nostr_relay_builder::local::LocalRelay` (the same fixture the engine's
//! own `crates/nmp/tests/runtime_integration.rs` drives) with two things a
//! plain `LocalRelay` doesn't give a caller:
//!
//! - **behavior knobs** (`reject_writes`, `reject_queries` — a `Given` about
//!   the world, per the approach doc §2.3), wired through `LocalRelay`'s own
//!   `WritePolicy`/`QueryPolicy` plugin points;
//! - **a world-side wire recorder** ([`WireRecord`]) that decodes every
//!   `REQ`/`CLOSE` a client actually put on the socket, subscription id and
//!   all -- see the `wire observation` section below for why the library's
//!   `QueryPolicy` hook cannot answer that question;
//! - **a world-side "was this relay ever contacted" observable**
//!   (`ScriptedRelay::contacted`/`contact_count`/`wait_contacted`), bumped by
//!   those SAME policy hooks on every inbound EVENT/REQ. This is
//!   deliberately independent of the engine's own `DiagnosticsSnapshot`: a
//!   `must-never` scenario asserting "no relay outside the plan was ever
//!   contacted" must not take the engine's self-report as its only witness,
//!   or a diagnostics bug could silently make the ledger scenario
//!   un-falsifiable. `wait_contacted` is the same log's BOUNDED-WAIT half
//!   (#60): a freshly rebound relay instance (`start_on_port`, used by the
//!   reconnect scenario) starts its own count at zero, so blocking on its
//!   first contact is a deterministic "the engine's `Pool` reconnected and
//!   resubscribed here" signal, rather than a fixed-timeout guess racing an
//!   unrelated, run-to-run-varying backoff delay.
//!
//! Deliberately NOT a glob import of `nostr_relay_builder::prelude::*` in
//! the signature-facing parts of this module: that re-exports a DIFFERENT
//! `nostr` (0.45-alpha) than this workspace's pinned `nostr = "0.44.4"` --
//! see `crates/nmp/tests/runtime_integration.rs`'s identical comment. Every
//! cross-version value (keypairs, seeded events) is bridged explicitly by
//! hex/id string round-trip (`mirror_keys`).

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::{ClientTap, ClientTapFactory, ConnectionOwner};
use nostr::{JsonUtil, RelayUrl};

use nostr_relay_builder::builder::{
    LocalRelayBuilder, LocalRelayBuilderNip42, QueryPolicy, QueryPolicyResult, WritePolicy,
    WritePolicyResult,
};
use nostr_relay_builder::local::LocalRelay;
use nostr_relay_builder::prelude::{
    Event as RelayEvent, EventBuilder as RelayEventBuilder, FinalizeEvent, Keys as RelayKeys,
    MachineReadablePrefix,
};

/// World-side config staged by `Given` steps BEFORE the relay is actually
/// bound/started (relays are started lazily -- see `NmpWorld::ensure_started`
/// -- so a scenario can compose several `Given`s about the same relay in any
/// order before anything hits a real socket).
#[derive(Debug, Clone, Default)]
pub struct RelayConfig {
    pub reject_writes: bool,
    /// Approximates "never confirms end of stored events": the relay
    /// refuses the query outright (`CLOSED`, never `EOSE`), which yields the
    /// same app-observable consequence the ledger scenario cares about --
    /// this relay's coverage for any query touching it never resolves out
    /// of `Unknown`. See the module doc's contacted-log note for why this
    /// is a deliberate, documented approximation rather than a true
    /// accept-but-never-EOSE relay (that behavior is not a plugin point
    /// `nostr-relay-builder` 0.45.0-alpha.3 exposes).
    pub reject_queries: bool,
    /// NIP-42 write gating (`LocalRelayBuilderNip42::write()`). Verified
    /// behavior of `LocalRelay` 0.45.0-alpha.3 in this mode: it does NOT
    /// challenge on connect; on an unauthenticated EVENT it sends
    /// `["AUTH", challenge]` followed by
    /// `["OK", id, false, "auth-required: you must auth"]`. Reads are NOT
    /// gated. Defaults to `false` so every existing scenario keeps its
    /// ungated relay semantics.
    pub auth_required_writes: bool,
    /// What this relay publishes in its NIP-11 document. `None` -- the
    /// default, and every pre-existing scenario's behaviour -- means it
    /// publishes NO document at all: an HTTP fetch is answered `404`, which
    /// is exactly what relay.nostr.band and relay.snort.social do today.
    pub advertised_limits: Option<AdvertisedLimits>,
}

/// The `limitation` fields a scripted relay advertises. Each is optional
/// because NIP-11 omission means "said nothing", never an implicit zero --
/// and a document that carries a `limitation` object WITHOUT
/// `max_subscriptions` is a real shape worth being able to script.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdvertisedLimits {
    pub max_subscriptions: Option<u64>,
    pub max_subid_length: Option<u64>,
}

impl AdvertisedLimits {
    /// The exact JSON document this relay serves. Hand-built rather than
    /// serialized from a type, so the test corpus states the wire shape it
    /// means -- the same reason the wire assertions read raw client bytes.
    fn document(&self) -> String {
        let mut limitation = Vec::new();
        if let Some(value) = self.max_subscriptions {
            limitation.push(format!("\"max_subscriptions\":{value}"));
        }
        if let Some(value) = self.max_subid_length {
            limitation.push(format!("\"max_subid_length\":{value}"));
        }
        format!(
            "{{\"name\":\"scripted\",\"supported_nips\":[1,11],\"limitation\":{{{}}}}}",
            limitation.join(",")
        )
    }
}

/// Contact count + a `Notify` so a caller can WAIT (bounded, no spin-poll --
/// same idiom as `NmpWorld`'s `FeedState`/`ReceiptState`/`DiagFeed`) for the
/// NEXT contact rather than guessing how long one takes to arrive. This is
/// what makes the reconnect scenario's "relay comes back" step deterministic
/// (#60): instead of assuming the engine's `Pool` has already reconnected and
/// resubscribed by some fixed wall-clock offset, the world can wait for THIS
/// relay instance's OWN evidence that a REQ/EVENT actually reached it again.
#[derive(Debug, Default)]
struct ContactLog {
    count: AtomicU64,
    notify: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct QueryLog {
    by_kind: Mutex<BTreeMap<u16, u64>>,
}

impl QueryLog {
    fn record(&self, query: &nostr_relay_builder::prelude::Filter) {
        let value = serde_json::to_value(query)
            .expect("nmp-bdd: scripted relay query must serialize for observation");
        let Some(kinds) = value.get("kinds").and_then(serde_json::Value::as_array) else {
            return;
        };
        let mut counts = self
            .by_kind
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for kind in kinds.iter().filter_map(serde_json::Value::as_u64) {
            *counts.entry(kind as u16).or_default() += 1;
        }
    }

    fn count(&self, kind: u16) -> u64 {
        self.by_kind
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(&kind)
            .copied()
            .unwrap_or(0)
    }
}

impl ContactLog {
    fn record(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn count(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }

    /// Bounded wait for the count to become nonzero -- never a spin-poll
    /// loop. Just pinning `Notify::notified()`'s returned future does NOT
    /// yet register it as a waiter (that only happens once the future is
    /// polled), so a `record()` -> `notify_waiters()` racing between the
    /// second `count()` check and this future's first poll would otherwise
    /// notify zero waiters and be silently lost -- the classic `Notify`
    /// lost-wakeup trap. `Notified::enable()` is Tokio's documented fix:
    /// it registers the waiter immediately, without consuming a poll, so
    /// calling it BEFORE the second check closes that exact window.
    async fn wait_contacted(&self, timeout: Duration) -> bool {
        if self.count() > 0 {
            return true;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.count() > 0 {
            return true;
        }
        tokio::time::timeout(timeout, notified).await.is_ok()
    }
}

// ---- wire observation ------------------------------------------------
//
// `contacted`/`query_count_for_kind` above answer "was this relay asked
// anything, and about which kind". They cannot answer the question this
// crate's subscription-collapse scenarios are ABOUT: how many distinct
// SUBSCRIPTIONS a set of demands compiled into, and whether a REQ opened a
// new one or replaced a live one in place. `nostr-relay-builder`'s
// `QueryPolicy` hook cannot answer it either -- it is invoked once per
// FILTER, never sees the subscription id, and only after the relay has
// already rewritten `filter.limit` out from under the observation.
//
// So this half of the observation sits one layer lower, on the raw
// client-to-relay byte stream that [`ConnectionOwner`] already forwards
// (`ClientTapFactory`). It decodes exactly the frames NIP-01 defines --
// `["REQ", <sub-id>, <filter>...]` and `["CLOSE", <sub-id>]` -- and is
// therefore a witness to what NMP literally put on the socket, entirely
// independent of both the engine's diagnostics and the relay library's
// own bookkeeping. Same discipline as the contacted-log (see the module
// doc): a scenario claiming "one subscription, not eight" must not take
// the thing under test as its only witness.

/// One REQ a client put on a relay's socket.
#[derive(Debug, Clone)]
pub struct WireReq {
    /// The NIP-01 subscription id this REQ names.
    pub sub_id: String,
    /// The REQ's filters, verbatim, exactly as they crossed the socket.
    pub filters: Vec<serde_json::Value>,
    /// `true` iff `sub_id` was ALREADY live when this REQ arrived -- a
    /// NIP-01 in-place filter REPLACEMENT (the shape the author axis uses
    /// to widen without churn), rather than a newly opened subscription.
    pub replaces: bool,
}

impl WireReq {
    /// Every value this REQ asks for under single-letter tag `tag` (`#p`,
    /// `#d`, ...), unioned across its filters.
    pub fn tag_values(&self, tag: char) -> BTreeSet<String> {
        let key = format!("#{tag}");
        self.filters
            .iter()
            .filter_map(|f| f.get(&key))
            .filter_map(serde_json::Value::as_array)
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect()
    }

    /// True iff any of this REQ's filters constrains tag `tag` at all.
    pub fn names_tag(&self, tag: char) -> bool {
        let key = format!("#{tag}");
        self.filters.iter().any(|f| f.get(&key).is_some())
    }

    /// Every single-letter tag name this REQ constrains.
    pub fn tag_names(&self) -> BTreeSet<char> {
        self.filters
            .iter()
            .filter_map(serde_json::Value::as_object)
            .flat_map(|obj| obj.keys())
            .filter_map(|k| {
                let mut chars = k.chars();
                match (chars.next(), chars.next(), chars.next()) {
                    (Some('#'), Some(c), None) => Some(c),
                    _ => None,
                }
            })
            .collect()
    }

    /// Every author this REQ asks for, unioned across its filters -- the
    /// control axis the tag axis is measured against.
    pub fn authors(&self) -> BTreeSet<String> {
        self.filters
            .iter()
            .filter_map(|f| f.get("authors"))
            .filter_map(serde_json::Value::as_array)
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect()
    }

    /// The largest `limit` any of this REQ's filters carries, or `None` if
    /// none of them is bounded.
    ///
    /// `max` rather than a set: the question these assertions ask is "did the
    /// wire promise more rows than the feed asked for", and one over-large
    /// filter is enough to answer yes. An absent `limit` is unbounded and is
    /// deliberately NOT reported as zero -- unbounded and "asks for nothing"
    /// are opposite conditions, and conflating them is how a bounded-feed
    /// assertion would pass against a filter that dropped its window.
    pub fn max_limit(&self) -> Option<u64> {
        self.filters
            .iter()
            .filter_map(|f| f.get("limit"))
            .filter_map(serde_json::Value::as_u64)
            .max()
    }

    /// Every kind this REQ asks for.
    pub fn kinds(&self) -> BTreeSet<u16> {
        self.filters
            .iter()
            .filter_map(|f| f.get("kinds"))
            .filter_map(serde_json::Value::as_array)
            .flatten()
            .filter_map(serde_json::Value::as_u64)
            .map(|k| k as u16)
            .collect()
    }

    /// True iff some filter in this REQ narrows by NOTHING but `kinds` --
    /// no tag, no author, no id. The shape the privacy floor forbids: an
    /// empty resolved value set must never widen into "send me everything
    /// of this kind".
    ///
    /// `limit` is tolerated because the relay injects one of its own before
    /// any observation downstream of the socket would see it. `since`/`until`
    /// deliberately are NOT: a demand-side time window still asks for every
    /// event of a kind within it, which is the same disclosure this floor
    /// exists to prevent.
    pub fn narrows_by_kind_alone(&self) -> bool {
        self.filters.iter().any(|f| {
            let Some(obj) = f.as_object() else {
                return false;
            };
            obj.keys().all(|k| matches!(k.as_str(), "kinds" | "limit"))
        })
    }
}

/// Everything one scripted relay saw a client send, in arrival order.
#[derive(Debug, Clone, Default)]
pub struct WireRecord {
    pub reqs: Vec<WireReq>,
    pub closes: Vec<String>,
}

impl WireRecord {
    /// Subscription ids this client REVIVED: named by a REQ, allowed to go
    /// dead, then named by a fresh REQ again (#932).
    ///
    /// `replaces` already distinguishes "this REQ replaced a subscription
    /// that was still live" (an in-place widen, which is normal and cheap)
    /// from "this REQ opened a subscription that was not live". A REQ of the
    /// second kind naming an id the relay has seen before is a revival — and
    /// a revival is what lets an answer the relay is still sending for the
    /// FIRST request be mistaken for the answer to the second.
    pub fn revived_subscription_ids(&self) -> Vec<String> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut revived = Vec::new();
        for req in &self.reqs {
            if !req.replaces && !seen.insert(req.sub_id.as_str()) {
                revived.push(req.sub_id.clone());
            }
        }
        revived
    }

    /// Distinct subscription ids, in first-seen order.
    pub fn subscription_ids(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut order = Vec::new();
        for req in &self.reqs {
            if seen.insert(req.sub_id.clone()) {
                order.push(req.sub_id.clone());
            }
        }
        order
    }

    /// Only the REQs that constrain tag `tag`. Every scenario about one tag
    /// axis reads through this, so an unrelated background REQ (a relay-list
    /// lookup, a discovery probe) can never inflate a "how many
    /// subscriptions" count.
    pub fn reqs_naming_tag(&self, tag: char) -> Vec<&WireReq> {
        self.reqs.iter().filter(|r| r.names_tag(tag)).collect()
    }

    /// Distinct subscription ids that ever carried a filter on tag `tag`,
    /// in first-seen order.
    pub fn subscription_ids_naming_tag(&self, tag: char) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut order = Vec::new();
        for req in self.reqs_naming_tag(tag) {
            if seen.insert(req.sub_id.clone()) {
                order.push(req.sub_id.clone());
            }
        }
        order
    }

    /// Distinct subscription ids that carried a filter on tag `tag` and have
    /// NOT since been closed -- what the relay is serving right now. The
    /// count-shaped assertions read this rather than every id ever seen: a
    /// subscription that was opened and closed again is churn (which
    /// `redundant_reqs`/`closes` witness separately), not something the relay
    /// is still serving.
    pub fn live_subscription_ids_naming_tag(&self, tag: char) -> Vec<String> {
        self.subscription_ids_naming_tag(tag)
            .into_iter()
            .filter(|id| !self.closes.contains(id))
            .collect()
    }

    /// Distinct subscription ids that ever carried an `authors` filter, in
    /// first-seen order -- the control axis's sibling of
    /// [`Self::subscription_ids_naming_tag`].
    pub fn subscription_ids_naming_authors(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut order = Vec::new();
        for req in self.reqs_naming_authors() {
            if seen.insert(req.sub_id.clone()) {
                order.push(req.sub_id.clone());
            }
        }
        order
    }

    /// The live-only sibling of [`Self::subscription_ids_naming_authors`].
    pub fn live_subscription_ids_naming_authors(&self) -> Vec<String> {
        self.subscription_ids_naming_authors()
            .into_iter()
            .filter(|id| !self.closes.contains(id))
            .collect()
    }

    /// Only the REQs that constrain `authors` -- the control axis.
    pub fn reqs_naming_authors(&self) -> Vec<&WireReq> {
        self.reqs
            .iter()
            .filter(|r| !r.authors().is_empty())
            .collect()
    }

    /// The LAST REQ sent on `sub_id` -- NIP-01 replacement means only the
    /// most recent filter set for a subscription is the live one.
    pub fn latest_req_on(&self, sub_id: &str) -> Option<&WireReq> {
        self.reqs.iter().rev().find(|r| r.sub_id == sub_id)
    }

    /// Subscription ids opened and not since closed.
    pub fn live_subscription_ids(&self) -> Vec<String> {
        self.subscription_ids()
            .into_iter()
            .filter(|id| !self.closes.contains(id))
            .collect()
    }

    /// REQs that re-sent a subscription's EXISTING filter verbatim -- same
    /// subscription id, byte-identical filter set as that id's previous REQ.
    /// A replacement that changes nothing is pure wire cost: the relay
    /// re-runs the query and re-streams whatever it matched.
    pub fn redundant_reqs(&self) -> Vec<&WireReq> {
        let mut latest: BTreeMap<&str, &Vec<serde_json::Value>> = BTreeMap::new();
        let mut redundant = Vec::new();
        for req in &self.reqs {
            if latest.insert(req.sub_id.as_str(), &req.filters) == Some(&req.filters) {
                redundant.push(req);
            }
        }
        redundant
    }

    /// How many REQs re-used an already-live subscription id (widened or
    /// shrank it in place) rather than opening a new one.
    pub fn replacement_count(&self) -> usize {
        self.reqs.iter().filter(|r| r.replaces).count()
    }
}

/// Decoded REQ/CLOSE log plus a monotonic frame counter (the quiescence
/// signal every count-shaped assertion settles against) and a fault list.
#[derive(Debug, Default)]
struct WireLog {
    inner: Mutex<WireLogInner>,
}

#[derive(Debug, Default)]
struct WireLogInner {
    reqs: Vec<WireReq>,
    closes: Vec<String>,
    live: BTreeSet<String>,
    frames: u64,
    /// Anything the decoder could not honestly account for. Surfaced as a
    /// PANIC at read time rather than swallowed: a silently dropped frame
    /// would turn a red scenario green, which is the one failure mode this
    /// whole spec exists to prevent.
    faults: Vec<String>,
}

impl WireLog {
    fn record_message(&self, message: &serde_json::Value) {
        let Some(array) = message.as_array() else {
            return;
        };
        let Some(verb) = array.first().and_then(serde_json::Value::as_str) else {
            return;
        };
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match verb {
            "REQ" => {
                let Some(sub_id) = array.get(1).and_then(serde_json::Value::as_str) else {
                    inner
                        .faults
                        .push(format!("REQ without a subscription id: {message}"));
                    return;
                };
                let replaces = inner.live.contains(sub_id);
                inner.live.insert(sub_id.to_string());
                inner.frames += 1;
                inner.reqs.push(WireReq {
                    sub_id: sub_id.to_string(),
                    filters: array[2..].to_vec(),
                    replaces,
                });
            }
            "CLOSE" => {
                let Some(sub_id) = array.get(1).and_then(serde_json::Value::as_str) else {
                    inner
                        .faults
                        .push(format!("CLOSE without a subscription id: {message}"));
                    return;
                };
                inner.live.remove(sub_id);
                inner.frames += 1;
                inner.closes.push(sub_id.to_string());
            }
            // EVENT/AUTH/COUNT are already witnessed by the contacted-log;
            // nothing in this crate asserts on their wire shape.
            _ => {}
        }
    }

    fn fault(&self, what: String) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .faults
            .push(what);
    }

    fn frames(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).frames
    }

    fn snapshot(&self) -> WireRecord {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            inner.faults.is_empty(),
            "nmp-bdd: the relay's client-frame decoder could not account for \
             every frame, so no count read from it is trustworthy: {:?}",
            inner.faults
        );
        WireRecord {
            reqs: inner.reqs.clone(),
            closes: inner.closes.clone(),
        }
    }
}

/// Per-connection websocket reassembler over the raw client-to-relay bytes.
///
/// Deliberately minimal, and LOUD about everything it does not handle. NMP's
/// own client is `tungstenite` 0.29 built with `default-features = false`
/// (`handshake` + rustls only), and tungstenite implements no compression
/// extension at all, so these frames are always unfragmented, masked, plain
/// text. Rather than trust that reasoning, every assumption is checked at
/// RUNTIME and a violation is recorded as a fault, not skipped.
struct ClientFrames {
    log: Arc<WireLog>,
    buf: Vec<u8>,
    handshake_done: bool,
}

impl ClientFrames {
    fn new(log: Arc<WireLog>) -> Self {
        Self {
            log,
            buf: Vec::new(),
            handshake_done: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        if !self.handshake_done && !self.skip_handshake() {
            return;
        }
        while self.decode_one_frame() {}
    }

    /// Consume the client's HTTP upgrade request. Returns `false` while it is
    /// still incomplete.
    fn skip_handshake(&mut self) -> bool {
        let Some(end) = self
            .buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| i + 4)
        else {
            return false;
        };
        let headers = String::from_utf8_lossy(&self.buf[..end]).to_lowercase();
        if headers.contains("sec-websocket-extensions") {
            // A negotiated extension (permessage-deflate) would make the
            // payloads below compressed rather than plain JSON, and this
            // decoder would silently see nothing.
            self.log.fault(
                "client negotiated a websocket extension; frame payloads are no longer plain text"
                    .to_string(),
            );
        }
        self.buf.drain(..end);
        self.handshake_done = true;
        true
    }

    /// Decode ONE frame off the front of the buffer. Returns `false` when the
    /// buffer does not yet hold a whole frame.
    fn decode_one_frame(&mut self) -> bool {
        if self.buf.len() < 2 {
            return false;
        }
        let fin = self.buf[0] & 0x80 != 0;
        let opcode = self.buf[0] & 0x0f;
        let masked = self.buf[1] & 0x80 != 0;
        let short_len = (self.buf[1] & 0x7f) as usize;

        let (payload_len, mut offset) = match short_len {
            126 => {
                if self.buf.len() < 4 {
                    return false;
                }
                (
                    u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize,
                    4usize,
                )
            }
            127 => {
                if self.buf.len() < 10 {
                    return false;
                }
                let mut be = [0u8; 8];
                be.copy_from_slice(&self.buf[2..10]);
                (u64::from_be_bytes(be) as usize, 10usize)
            }
            n => (n, 2usize),
        };

        let mask = if masked {
            if self.buf.len() < offset + 4 {
                return false;
            }
            let mask = [
                self.buf[offset],
                self.buf[offset + 1],
                self.buf[offset + 2],
                self.buf[offset + 3],
            ];
            offset += 4;
            Some(mask)
        } else {
            None
        };

        let total = offset + payload_len;
        if self.buf.len() < total {
            return false;
        }
        let mut payload = self.buf[offset..total].to_vec();
        if let Some(mask) = mask {
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[i % 4];
            }
        }
        self.buf.drain(..total);

        match opcode {
            // Text -- the only frame NIP-01 traffic uses.
            0x1 => {
                if !fin {
                    self.log.fault(
                        "fragmented client text frame: this decoder reassembles no continuations"
                            .to_string(),
                    );
                    return true;
                }
                match serde_json::from_slice::<serde_json::Value>(&payload) {
                    Ok(message) => self.log.record_message(&message),
                    Err(error) => self
                        .log
                        .fault(format!("client text frame is not JSON: {error}")),
                }
            }
            // Continuation / binary: never produced by this client, and
            // silently ignoring either could hide a whole REQ.
            0x0 | 0x2 => self
                .log
                .fault(format!("unexpected client frame opcode {opcode:#x}")),
            // Close/ping/pong carry no NIP-01 message.
            0x8..=0xa => {}
            other => self
                .log
                .fault(format!("unknown client frame opcode {other:#x}")),
        }
        true
    }
}

/// One running in-process relay, its contacted-log, and its wire recorder.
pub struct ScriptedRelay {
    pub url: RelayUrl,
    port: u16,
    relay: LocalRelay,
    connection_owner: Option<ConnectionOwner>,
    contacted: Arc<ContactLog>,
    queries: Arc<QueryLog>,
    wire: Arc<WireLog>,
    connections: Arc<AtomicU64>,
    admitted: Arc<Mutex<Vec<nostr::Event>>>,
}

impl ScriptedRelay {
    /// Bind a fresh ephemeral port and start a `LocalRelay` configured per
    /// `config`. Async because `LocalRelay::run` is (it needs the ambient
    /// tokio runtime `tests/bdd.rs`'s `#[tokio::main]` provides).
    pub async fn start(config: &RelayConfig) -> Self {
        Self::start_on_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), config).await
    }

    /// Start a `LocalRelay` on a SPECIFIC port -- the reconnect/drop-and-
    /// come-back scenarios' "relay X comes back" step rebinds the exact
    /// port a just-shut-down relay used (`self.port`), the same trick
    /// `crates/nmp/tests/runtime_integration.rs` uses, so the engine's own
    /// `Pool` reconnects to the SAME `RelayUrl` it already had open.
    pub async fn start_on_port(port: u16, config: &RelayConfig) -> Self {
        Self::start_on_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, port)), config).await
    }

    async fn start_on_addr(public_addr: SocketAddr, config: &RelayConfig) -> Self {
        let contacted = Arc::new(ContactLog::default());
        let admitted: Arc<Mutex<Vec<nostr::Event>>> = Arc::new(Mutex::new(Vec::new()));
        let queries = Arc::new(QueryLog::default());
        let wire = Arc::new(WireLog::default());
        let backend_port = free_port();

        let mut builder = LocalRelayBuilder::default()
            .addr(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .port(backend_port)
            .write_policy(LoggingWritePolicy {
                contacted: contacted.clone(),
                admitted: admitted.clone(),
                reject: config.reject_writes,
            })
            .query_policy(LoggingQueryPolicy {
                contacted: contacted.clone(),
                queries: queries.clone(),
                reject: config.reject_queries,
            });
        if config.auth_required_writes {
            builder = builder.nip42(LocalRelayBuilderNip42::write());
        }
        let relay = builder.build();
        relay
            .run()
            .await
            .expect("nmp-bdd: scripted relay must start");
        let tap_log = Arc::clone(&wire);
        let connections = Arc::new(AtomicU64::new(0));
        let tap_connections = Arc::clone(&connections);
        let tap: ClientTapFactory = Arc::new(move || {
            // The factory is called once per ACCEPTED connection (see
            // `ClientTapFactory`'s doc), which makes this the world-side
            // "how many times has a client connected here" witness -- what
            // tells a reconnect-replay REQ apart from a recompiled one.
            tap_connections.fetch_add(1, Ordering::Relaxed);
            let mut frames = ClientFrames::new(Arc::clone(&tap_log));
            Box::new(move |bytes: &[u8]| frames.push(bytes)) as ClientTap
        });
        let connection_owner = ConnectionOwner::bind_with_tap_and_document(
            public_addr,
            SocketAddr::from((Ipv4Addr::LOCALHOST, backend_port)),
            Some(tap),
            config
                .advertised_limits
                .as_ref()
                .map(AdvertisedLimits::document),
        )
        .await
        .expect("nmp-bdd: client-facing relay owner must bind");
        let public_addr = connection_owner.local_addr();
        let url = RelayUrl::parse(&format!("ws://{public_addr}"))
            .expect("nmp-bdd: client-facing relay URL must parse");

        Self {
            url,
            port: public_addr.port(),
            relay,
            connection_owner: Some(connection_owner),
            contacted,
            queries,
            wire,
            connections,
            admitted,
        }
    }

    /// Every EVENT kind this relay's write policy has admitted, in arrival
    /// order -- the other half of the contacted-log (which counts REQ and
    /// EVENT alike without saying which).
    pub fn admitted_event_kinds(&self) -> Vec<u16> {
        self.admitted_events()
            .iter()
            .map(|event| event.kind.as_u16())
            .collect()
    }

    /// Every EVENT this relay's write policy has admitted, whole and in
    /// arrival order. The kinds above are a projection of this; an
    /// assertion about WHO published something (the identity plane) needs
    /// the author, and the author only exists on the event itself.
    pub fn admitted_events(&self) -> Vec<nostr::Event> {
        self.admitted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// How many client connections this address has ACCEPTED, ever --
    /// websocket sessions and NIP-11 document fetches alike (one accept is
    /// one connection either way). A second accept after the engine was
    /// already talking to this relay is a RECONNECT, and a reconnect
    /// replays the whole live req list (`apply_replay`).
    pub fn connection_count(&self) -> u64 {
        self.connections.load(Ordering::Relaxed)
    }

    /// The port this relay is (or was, if since shut down) bound to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Seed a kind:1 text note directly into this relay's own database --
    /// pre-existing protocol state a `Given` stages, never a "when" (a live
    /// note goes through the engine's own `Handle::publish`, not this
    /// method).
    pub async fn seed_note(&self, author: &nostr::Keys, content: &str, created_at: u64) {
        let relay_keys = mirror_keys(author);
        let event: RelayEvent = RelayEventBuilder::text_note(content)
            .custom_created_at(nostr_relay_builder::prelude::Timestamp::from(created_at))
            .finalize(&relay_keys)
            .expect("nmp-bdd: fixture note must sign cleanly");
        self.relay
            .add_event(event)
            .await
            .expect("nmp-bdd: seeding a fixture note must succeed");
    }

    /// Seed a kind:3 (contact list) event -- `Given <person> follows
    /// <people>`'s pre-existing-state half (the reactive re-route
    /// scenarios' `When` publishes a NEW one live, through the engine).
    pub async fn seed_contact_list(
        &self,
        author: &nostr::Keys,
        follows: &[nostr::PublicKey],
        created_at: u64,
    ) {
        let relay_keys = mirror_keys(author);
        let tags = follows.iter().map(|pk| {
            let hex_pk = nostr_relay_builder::prelude::PublicKey::parse(&pk.to_hex())
                .expect("nmp-bdd: bridge follow pubkey across nostr crate versions");
            nostr_relay_builder::prelude::Tag::public_key(hex_pk)
        });
        let event: RelayEvent =
            RelayEventBuilder::new(nostr_relay_builder::prelude::Kind::ContactList, "")
                .tags(tags)
                .custom_created_at(nostr_relay_builder::prelude::Timestamp::from(created_at))
                .finalize(&relay_keys)
                .expect("nmp-bdd: fixture contact list must sign cleanly");
        self.relay
            .add_event(event)
            .await
            .expect("nmp-bdd: seeding a fixture contact list must succeed");
    }

    /// Seed an already-signed workspace event verbatim into this relay.
    /// JSON round-tripping bridges the two pinned `nostr` crate versions
    /// without re-signing, which lets product-level parity tests compare
    /// every raw row token (including the signature) exactly.
    pub async fn seed_signed_event(&self, event: &nostr::Event) {
        let event: RelayEvent = serde_json::from_str(&event.as_json())
            .expect("nmp-bdd: bridge signed fixture across nostr crate versions");
        self.relay
            .add_event(event)
            .await
            .expect("nmp-bdd: seeding a fixture event must succeed");
    }

    /// Seed the author's NIP-65 relay list, naming this relay as an
    /// unmarked read+write relay. The facade parity scenario uses the real
    /// discovery path before publishing; it never injects a mechanism-level
    /// directory fixture into either product surface.
    pub async fn seed_own_relay_list(&self, author: &nostr::Keys, created_at: u64) {
        let relay_keys = mirror_keys(author);
        let relay_url = self.url.to_string();
        let event = RelayEventBuilder::new(nostr_relay_builder::prelude::Kind::RelayList, "")
            .tag(
                nostr_relay_builder::prelude::Tag::parse(["r".to_string(), relay_url])
                    .expect("nmp-bdd: relay-list fixture tag must parse"),
            )
            .custom_created_at(nostr_relay_builder::prelude::Timestamp::from(created_at))
            .finalize(&relay_keys)
            .expect("nmp-bdd: relay-list fixture must sign cleanly");
        self.relay
            .add_event(event)
            .await
            .expect("nmp-bdd: seeding a relay-list fixture must succeed");
    }

    /// True iff this relay's write or query policy has been invoked at
    /// least once -- i.e. some REQ or EVENT actually reached it. The
    /// world-side half of a `must-never` "no relay outside the plan was
    /// ever contacted" assertion (see the module doc).
    pub fn contacted(&self) -> bool {
        self.contacted.count() > 0
    }

    /// How many times this relay's write/query policy has been invoked --
    /// the finer-grained sibling of [`Self::contacted`], used by the
    /// "untouched" assertion (a relay whose count hasn't moved since a
    /// snapshot received no NEW REQ/EVENT at all).
    pub fn contact_count(&self) -> u64 {
        self.contacted.count()
    }

    /// Number of inbound REQs admitted to this relay's `QueryPolicy` whose
    /// filter named `kind` (whether the policy later accepts or rejects the
    /// query). This relay-side admission witness is independent of engine
    /// diagnostics; parity tests conjunct it with engine event counts to
    /// prove every admitted fixture response was processed before advancing.
    pub fn query_count_for_kind(&self, kind: u16) -> u64 {
        self.queries.count(kind)
    }

    /// Every `REQ`/`CLOSE` this relay's client has sent so far, decoded from
    /// the raw socket bytes -- subscription ids included. Panics if the
    /// decoder ever failed to account for a frame (see [`WireLog::snapshot`]):
    /// a count read off an incomplete record would be worse than no count.
    pub fn wire_record(&self) -> WireRecord {
        self.wire.snapshot()
    }

    /// Block until this relay's client has sent NOTHING for a whole `quiet`
    /// window (or `max` elapses). Nearly every subscription-collapse
    /// assertion is a COUNT ("exactly one subscription", "two distinct
    /// ones", "no CLOSE"), and a count is only meaningful against a settled
    /// wire: recompilation is driven by ingested rows, so REQs keep arriving
    /// for as long as demand is still resolving. Coarse by design -- one
    /// `sleep` per quiet window, comparing a monotonic frame counter, not a
    /// spin-poll.
    pub async fn wait_wire_quiet(&self, quiet: Duration, max: Duration) {
        let deadline = Instant::now() + max;
        loop {
            let before = self.wire.frames();
            tokio::time::sleep(quiet).await;
            if self.wire.frames() == before || Instant::now() >= deadline {
                return;
            }
        }
    }

    /// Bounded wait (no spin-poll -- see [`ContactLog::wait_contacted`]) for
    /// this relay instance to be contacted at least once. The deterministic
    /// "has the engine's `Pool` actually reconnected and resubscribed here
    /// yet" signal the reconnect scenario needs (#60): a freshly rebound
    /// instance's `ContactLog` starts at zero, so the first contact IS the
    /// resubscribe-after-reconnect event, whenever it actually lands --
    /// never a guess at how long that takes.
    pub async fn wait_contacted(&self, timeout: Duration) -> bool {
        self.contacted.wait_contacted(timeout).await
    }

    /// Stop the backend accept loop during ordinary fixture cleanup.
    pub fn shutdown(&self) {
        self.relay.shutdown();
    }

    /// Synchronously sever the exact client-facing listener and every
    /// established stream, then stop the backend. The reconnect scenario can
    /// now rebind the public port without assuming `LocalRelay::shutdown()`
    /// closes sessions (it only stops the backend accept loop).
    pub async fn disconnect(&mut self) {
        self.connection_owner
            .take()
            .expect("nmp-bdd: relay connection owner must still be live")
            .shutdown()
            .await
            .expect("nmp-bdd: relay connection owner must shut down cleanly");
        self.relay.shutdown();
    }
}

/// Re-derive the identical keypair under `nostr-relay-builder`'s OWN
/// (0.45-alpha) `nostr` dependency, so an event seeded directly into a
/// scripted relay is attributable to the SAME author the engine (0.44.4
/// `nostr`) knows about -- identical bridge to
/// `runtime_integration.rs::mirror_keys`.
fn mirror_keys(k: &nostr::Keys) -> RelayKeys {
    RelayKeys::parse(&k.secret_key().to_secret_hex())
        .expect("nmp-bdd: mirror keypair across nostr crate versions")
}

/// Reserve an ephemeral backend port for `LocalRelay`. The client-visible
/// port is owned independently by [`ConnectionOwner`].
pub fn free_port() -> u16 {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .expect("nmp-bdd: bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

#[derive(Debug)]
struct LoggingWritePolicy {
    contacted: Arc<ContactLog>,
    admitted: Arc<Mutex<Vec<nostr::Event>>>,
    reject: bool,
}

impl WritePolicy for LoggingWritePolicy {
    fn admit_event<'a>(
        &'a self,
        event: &'a nostr_relay_builder::prelude::Event,
        _addr: &'a SocketAddr,
    ) -> nostr_relay_builder::prelude::BoxedFuture<'a, WritePolicyResult> {
        self.contacted.record();
        // Bridged across the two pinned `nostr` versions by JSON round-trip,
        // exactly as `seed_signed_event` does in the other direction (see
        // this module's dependency comment). Nothing is re-signed: the
        // admitted bytes are the bytes the client sent, so an assertion
        // about WHO published something reads the real author.
        {
            let json = serde_json::to_string(event)
                .expect("nmp-bdd: an admitted event always renders as JSON");
            let bridged: nostr::Event = serde_json::from_str(&json)
                .expect("nmp-bdd: bridge an admitted event across nostr crate versions");
            self.admitted
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(bridged);
        }
        let reject = self.reject;
        Box::pin(async move {
            if reject {
                WritePolicyResult::reject(
                    MachineReadablePrefix::Blocked,
                    "nmp-bdd scripted relay: configured to reject every event",
                )
            } else {
                WritePolicyResult::Accept
            }
        })
    }
}

#[derive(Debug)]
struct LoggingQueryPolicy {
    contacted: Arc<ContactLog>,
    queries: Arc<QueryLog>,
    reject: bool,
}

impl QueryPolicy for LoggingQueryPolicy {
    fn admit_query<'a>(
        &'a self,
        query: &'a mut nostr_relay_builder::prelude::Filter,
        _addr: &'a SocketAddr,
    ) -> nostr_relay_builder::prelude::BoxedFuture<'a, QueryPolicyResult> {
        self.contacted.record();
        self.queries.record(query);
        let reject = self.reject;
        Box::pin(async move {
            if reject {
                QueryPolicyResult::reject(
                    MachineReadablePrefix::Error,
                    "nmp-bdd scripted relay: configured to never confirm end of stored events",
                )
            } else {
                QueryPolicyResult::Accept
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Build the client-to-server websocket frame `payload` would be sent
    /// as: FIN + text opcode, masked (RFC 6455 requires client masking, and
    /// tungstenite does mask), 7-bit length.
    fn masked_text_frame(payload: &str) -> Vec<u8> {
        let bytes = payload.as_bytes();
        assert!(
            bytes.len() < 126,
            "test fixture frames stay in the 7-bit length form"
        );
        let mask = [0xa1u8, 0x0b, 0xc3, 0x5d];
        let mut frame = vec![0x81, 0x80 | bytes.len() as u8];
        frame.extend_from_slice(&mask);
        frame.extend(bytes.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        frame
    }

    /// One plain HTTP GET on the relay's own address returns its NIP-11
    /// document, and the SAME address still accepts websocket clients -- the
    /// two-protocols-one-port shape every real relay has, and the reason an
    /// acceptance test can drive the engine's real document acquisition.
    ///
    /// The falsifier for the other half is right below it: a relay with no
    /// advertisement answers 404, exactly as the two public relays measured
    /// for issue #931 do.
    #[tokio::test]
    async fn a_scripted_relay_serves_its_nip11_document_over_plain_http() {
        async fn fetch(relay: &ScriptedRelay) -> String {
            let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", relay.port()))
                .await
                .expect("relay address accepts a plain TCP client");
            socket
                .write_all(
                    b"GET / HTTP/1.1\r\nHost: localhost\r\n\
                      Accept: application/nostr+json\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("the request is writable");
            let mut response = Vec::new();
            socket
                .read_to_end(&mut response)
                .await
                .expect("the relay answers and closes");
            String::from_utf8(response).expect("the response is text")
        }

        let advertised = ScriptedRelay::start(&RelayConfig {
            advertised_limits: Some(AdvertisedLimits {
                max_subscriptions: Some(20),
                max_subid_length: Some(71),
            }),
            ..RelayConfig::default()
        })
        .await;
        let response = fetch(&advertised).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(
            response.contains("application/nostr+json"),
            "the fetcher asks for this exact content type: {response}"
        );
        assert!(response.contains("\"max_subscriptions\":20"), "{response}");
        assert!(response.contains("\"max_subid_length\":71"), "{response}");
        // The websocket half of the same address is untouched.
        assert!(advertised.wire_record().subscription_ids().is_empty());
        advertised.shutdown();

        let silent = ScriptedRelay::start(&RelayConfig::default()).await;
        let response = fetch(&silent).await;
        assert!(
            response.starts_with("HTTP/1.1 404"),
            "a relay that publishes nothing must SAY nothing: {response}"
        );
        silent.shutdown();
    }

    /// The recorder's own falsifier: a REQ and a CLOSE, fed in through the
    /// same path a real client's bytes take (HTTP upgrade first, then
    /// frames), delivered ONE BYTE AT A TIME so every reassembly boundary is
    /// exercised. Proves the three things every scenario's counts rest on:
    /// the handshake is skipped, masked payloads are recovered, and a second
    /// REQ on a live subscription id is reported as a REPLACEMENT rather than
    /// a second subscription.
    #[test]
    fn decodes_subscription_ids_and_replacement_from_raw_client_bytes() {
        let log = Arc::new(WireLog::default());
        let mut frames = ClientFrames::new(Arc::clone(&log));

        let mut stream = Vec::new();
        stream
            .extend_from_slice(b"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\r\n");
        stream.extend(masked_text_frame(
            r##"["REQ","sub-a",{"kinds":[1],"#p":["x"]}]"##,
        ));
        stream.extend(masked_text_frame(
            r##"["REQ","sub-a",{"kinds":[1],"#p":["x","y"]}]"##,
        ));
        stream.extend(masked_text_frame(
            r##"["REQ","sub-b",{"kinds":[1],"#t":["z"]}]"##,
        ));
        stream.extend(masked_text_frame(r#"["CLOSE","sub-b"]"#));
        for byte in stream {
            frames.push(&[byte]);
        }

        let record = log.snapshot();
        assert_eq!(record.reqs.len(), 3);
        assert_eq!(record.subscription_ids(), vec!["sub-a", "sub-b"]);
        assert_eq!(record.subscription_ids_naming_tag('p'), vec!["sub-a"]);
        assert_eq!(record.subscription_ids_naming_tag('t'), vec!["sub-b"]);
        assert_eq!(record.replacement_count(), 1);
        assert!(!record.reqs[0].replaces);
        assert!(
            record.reqs[1].replaces,
            "a REQ re-using a live sub id replaces it"
        );
        assert_eq!(
            record.latest_req_on("sub-a").unwrap().tag_values('p'),
            BTreeSet::from(["x".to_string(), "y".to_string()])
        );
        assert_eq!(record.closes, vec!["sub-b"]);
        assert_eq!(record.live_subscription_ids(), vec!["sub-a"]);
        assert_eq!(record.reqs[2].tag_names(), BTreeSet::from(['t']));
        assert!(!record.reqs[0].narrows_by_kind_alone());
    }

    /// A REQ this decoder could not account for must POISON every later
    /// read, never be skipped -- a dropped frame silently deflates exactly
    /// the counts the subscription-collapse scenarios assert on.
    #[test]
    fn an_unaccountable_frame_poisons_every_later_read() {
        let log = Arc::new(WireLog::default());
        let mut frames = ClientFrames::new(Arc::clone(&log));
        frames.push(b"GET / HTTP/1.1\r\n\r\n");
        // Binary opcode, unmasked, empty payload: legal websocket, but not
        // something this client ever sends, so it is a fault by construction.
        frames.push(&[0x82, 0x00]);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| log.snapshot()));
        assert!(
            poisoned.is_err(),
            "an unaccounted frame must panic the reader"
        );
    }

    /// Falsifier for the lost-wakeup trap `wait_contacted` must close
    /// (caught pre-merge on #60/PR #72 by codex-nova review): a bare
    /// `tokio::pin!(notify.notified())` does NOT register the future as a
    /// waiter -- only polling (or `enable()`) does -- so a `record()`
    /// landing between the "already contacted?" recheck and this future's
    /// first poll would notify zero waiters under `notify_waiters()` and be
    /// silently dropped. This pins the actual contract `wait_contacted`
    /// relies on: call `enable()` first, and a contact recorded after that
    /// point but strictly before the future is ever polled must still be
    /// observed.
    #[tokio::test]
    async fn enabled_notified_future_observes_a_contact_recorded_before_its_first_poll() {
        let log = ContactLog::default();
        let notified = log.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        // Exactly the ordering `wait_contacted`'s internal race window
        // allows: contact recorded after `enable()`, strictly before the
        // future's first poll (this test never polled it before this line).
        log.record();

        tokio::time::timeout(Duration::from_millis(200), notified)
            .await
            .expect(
                "an enable()d waiter must still observe a contact recorded before its first poll",
            );
    }

    /// End-to-end sibling: `wait_contacted` itself must observe a
    /// GENUINELY concurrent `record()` (a second task on a real
    /// multi-thread runtime, not this same synchronous sequence) --
    /// repeated many times to build confidence against scheduler-dependent
    /// flakiness, the same reason #60's own reconnect fix was proven 5x
    /// back-to-back rather than once. Would have been flaky/failing
    /// intermittently against the pre-`enable()` version of `wait_contacted`.
    #[tokio::test(flavor = "multi_thread")]
    async fn wait_contacted_observes_a_concurrent_record_under_repeated_trials() {
        for _ in 0..200 {
            let log = Arc::new(ContactLog::default());
            let recorder = {
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    log.record();
                })
            };
            let seen = log.wait_contacted(Duration::from_millis(500)).await;
            recorder.await.expect("recorder task must not panic");
            assert!(seen, "wait_contacted missed a concurrent record()");
        }
    }
}
