//! The scenario-facing script: *on an inbound frame matching P, put these
//! frames on the socket, in this order, with these delays.*
//!
//! That one sentence is the whole mechanism. Everything a relay can do to a
//! client -- truncate silently, never terminate the stored phase, forge an
//! event, challenge mid-subscription, answer the upgrade with a login page --
//! is a list of [`Step`]s, so a scenario states what the relay does rather
//! than configuring which of a fixed set of misbehaviours it is having today.
//!
//! The six-knob `RelayConfig` this replaces could describe exactly six
//! things. Its two most-used knobs are one line each here: `reject_writes`
//! is `.on_event(Ev::any(), Reply::rejected("blocked: not admitted"))`, and
//! `reject_queries` is `.on_req(Req::any(), Reply::closed("error: no"))` --
//! and unlike the old `reject_queries`, the thing it was an admitted
//! approximation OF is also sayable: [`Reply::never_eose`].

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use nostr::{Event, PublicKey};

/// One inbound REQ, decoded.
#[derive(Debug, Clone)]
pub struct ReqFrame {
    /// Which client connection this REQ arrived on. A second connection is a
    /// reconnect -- or a second engine, which is what makes a
    /// concurrent-edit scenario able to answer one of them differently.
    pub connection: usize,
    pub sub_id: String,
    /// The REQ's filters, verbatim, exactly as they crossed the socket.
    pub raw_filters: Vec<serde_json::Value>,
    /// The same filters, parsed. A filter this crate's `nostr` cannot parse
    /// is absent here and present in `raw_filters`; matching against
    /// [`Req::custom`] over the raw form is always available.
    pub filters: Vec<nostr::Filter>,
    /// Zero-based index of this REQ among every REQ THIS RELAY has received,
    /// across every connection. Relay-wide for the same reason `on_nth_req`
    /// is: NMP reopens the socket whenever demand goes to zero, and a
    /// per-connection ordinal silently restarts when it does.
    pub index: usize,
}

impl ReqFrame {
    /// Every kind this REQ asks for, unioned across its filters.
    #[must_use]
    pub fn kinds(&self) -> BTreeSet<u16> {
        self.filters
            .iter()
            .filter_map(|f| f.kinds.as_ref())
            .flatten()
            .map(nostr::Kind::as_u16)
            .collect()
    }

    /// Every author this REQ asks for.
    #[must_use]
    pub fn authors(&self) -> BTreeSet<PublicKey> {
        self.filters
            .iter()
            .filter_map(|f| f.authors.as_ref())
            .flatten()
            .copied()
            .collect()
    }

    /// The largest `limit` any filter carries, or `None` if none is bounded.
    ///
    /// `max` rather than a set: the question is "did the wire promise more
    /// rows than the app asked for", and one over-large filter answers yes.
    /// An absent `limit` is unbounded and deliberately NOT reported as zero.
    #[must_use]
    pub fn max_limit(&self) -> Option<usize> {
        self.filters.iter().filter_map(|f| f.limit).max()
    }

    /// Every value this REQ asks for under single-letter tag `name`.
    #[must_use]
    pub fn tag_values(&self, name: char) -> BTreeSet<String> {
        let key = format!("#{name}");
        self.raw_filters
            .iter()
            .filter_map(|f| f.get(&key))
            .filter_map(serde_json::Value::as_array)
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect()
    }
}

/// A predicate over an inbound REQ.
#[derive(Clone)]
pub struct Req(Arc<dyn Fn(&ReqFrame) -> bool + Send + Sync>);

impl std::fmt::Debug for Req {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Req(<predicate>)")
    }
}

impl Req {
    #[must_use]
    pub fn any() -> Self {
        Self(Arc::new(|_| true))
    }

    #[must_use]
    pub fn kind(kind: u16) -> Self {
        Self(Arc::new(move |req| req.kinds().contains(&kind)))
    }

    #[must_use]
    pub fn author(author: PublicKey) -> Self {
        Self(Arc::new(move |req| req.authors().contains(&author)))
    }

    #[must_use]
    pub fn tag(name: char, value: impl Into<String>) -> Self {
        let value = value.into();
        Self(Arc::new(move |req| req.tag_values(name).contains(&value)))
    }

    #[must_use]
    pub fn sub_id(sub_id: impl Into<String>) -> Self {
        let sub_id = sub_id.into();
        Self(Arc::new(move |req| req.sub_id == sub_id))
    }

    /// Anything the named predicates do not say. The raw filters are on the
    /// frame, so nothing about a REQ is out of reach of a scenario.
    #[must_use]
    pub fn custom(predicate: impl Fn(&ReqFrame) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(predicate))
    }

    #[must_use]
    pub fn and(self, other: Self) -> Self {
        Self(Arc::new(move |req| self.0(req) && other.0(req)))
    }

    #[must_use]
    pub fn or(self, other: Self) -> Self {
        Self(Arc::new(move |req| self.0(req) || other.0(req)))
    }

    pub(crate) fn matches(&self, req: &ReqFrame) -> bool {
        (self.0)(req)
    }
}

/// A predicate over an inbound EVENT (a client write).
#[derive(Clone)]
pub struct Ev(Arc<dyn Fn(&Event) -> bool + Send + Sync>);

impl std::fmt::Debug for Ev {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ev(<predicate>)")
    }
}

impl Ev {
    #[must_use]
    pub fn any() -> Self {
        Self(Arc::new(|_| true))
    }

    #[must_use]
    pub fn kind(kind: u16) -> Self {
        Self(Arc::new(move |event| event.kind.as_u16() == kind))
    }

    #[must_use]
    pub fn author(author: PublicKey) -> Self {
        Self(Arc::new(move |event| event.pubkey == author))
    }

    #[must_use]
    pub fn custom(predicate: impl Fn(&Event) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(predicate))
    }

    #[must_use]
    pub fn and(self, other: Self) -> Self {
        Self(Arc::new(move |event| self.0(event) && other.0(event)))
    }

    pub(crate) fn matches(&self, event: &Event) -> bool {
        (self.0)(event)
    }
}

/// How many of the events matching a REQ's filters the relay actually serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Serve {
    /// Everything that matches, bounded only by the client's own `limit`.
    /// The honest relay.
    Everything,
    /// At most `n`, whatever the client's `limit` said. The client cannot
    /// distinguish this from "that is all there is".
    AtMost(usize),
}

/// One thing the relay does on the socket. Steps run in order.
#[derive(Debug, Clone)]
pub enum Step {
    /// Serve matching events from this relay's corpus, as `EVENT` frames on
    /// the triggering subscription.
    Stored(Serve),
    /// Serve these exact events, whatever the client asked for. The relay
    /// verifies nothing on the way out, which is what makes a filter
    /// mismatch, a forgery, and a bad signature all sayable.
    Events(Vec<Event>),
    /// Serve these exact JSON values as the event body of an `EVENT` frame --
    /// for bodies `nostr::Event` refuses to hold at all.
    EventsJson(Vec<serde_json::Value>),
    /// `["EOSE", <sub-id>]`.
    Eose,
    /// `["CLOSED", <sub-id>, <message>]`.
    Closed(String),
    /// `["NOTICE", <message>]`.
    Notice(String),
    /// `["AUTH", <challenge>]`.
    Auth(String),
    /// `["OK", <event-id>, <accepted>, <message>]`. Only meaningful in a
    /// reply to an inbound EVENT or AUTH, where the id comes from.
    Ok { accepted: bool, message: String },
    /// Add the inbound event to this relay's corpus, so a later REQ can serve
    /// it back. Its absence from a reply that still says `OK: true` is
    /// exactly "accepted the write and never served it".
    Ingest,
    /// One arbitrary text frame, unvalidated.
    Raw(String),
    /// Octets written straight onto the TCP socket with no websocket framing
    /// at all. Injected garbage, a hand-built frame, a half-frame.
    Bytes(Vec<u8>),
    /// Frame `payload` correctly, then write only its first `keep_bytes`.
    /// The client is left holding a frame header promising bytes that never
    /// arrive -- the ergonomic form of mid-frame truncation.
    PartialFrame { payload: String, keep_bytes: usize },
    /// The same cut, applied to a REAL `EVENT` frame on the triggering
    /// subscription: build `["EVENT", <sub-id>, <event>]` and write only its
    /// first `keep_bytes`.
    ///
    /// Its own step rather than [`Self::PartialFrame`] over a hand-written
    /// payload, because a scenario cannot know the subscription id it is
    /// answering -- and a truncation scenario whose frame names a
    /// subscription the client never opened is immune to its own mutation:
    /// serving that frame IN FULL changes nothing, so the test passes whether
    /// or not truncation works. `keep_bytes` at or beyond the whole frame
    /// sends it intact, which is exactly the falsifier.
    PartialEvent { event: Box<Event>, keep_bytes: usize },
    /// Wait before the next step.
    Delay(Duration),
    /// Write nothing more on this connection, ever, and hold the socket open.
    /// A client that waits on this relay waits forever.
    Stall,
    /// Drop the TCP connection with no websocket close.
    Disconnect,
    /// An orderly websocket close.
    Close { code: u16, reason: String },
}

/// The ordered program the relay runs when a rule fires.
///
/// Constructors name a whole behaviour; `then_*` composes. `Reply::stored()`
/// is the honest relay and is what an unscripted REQ gets.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub(crate) steps: Vec<Step>,
}

impl Reply {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // ---- read-side whole behaviours -----------------------------------

    /// Everything matching, then EOSE. The honest relay.
    #[must_use]
    pub fn stored() -> Self {
        Self::new().then_stored().then_eose()
    }

    /// Serve at most `n` of the matching events, then EOSE -- however many
    /// the client's `limit` asked for. The client cannot distinguish "that
    /// is all there is" from "that is all I gave you".
    #[must_use]
    pub fn truncate_at(n: usize) -> Self {
        Self::new().then(Step::Stored(Serve::AtMost(n))).then_eose()
    }

    /// Accept the REQ, stream every matching event, and never terminate the
    /// stored phase. The subscription stays open and the client is never told
    /// the relay has finished.
    #[must_use]
    pub fn never_eose() -> Self {
        Self::new().then_stored()
    }

    /// EOSE first, then more events on the same subscription.
    #[must_use]
    pub fn eose_then(events: Vec<Event>) -> Self {
        Self::new().then_eose().then_events(events)
    }

    /// EOSE with nothing served -- the relay says it has finished and holds
    /// nothing, whatever its corpus actually contains.
    #[must_use]
    pub fn nothing() -> Self {
        Self::new().then_eose()
    }

    /// Not one frame. The REQ is accepted and never answered at all.
    #[must_use]
    pub fn silence() -> Self {
        Self::new()
    }

    /// Serve exactly these events, then EOSE. Whatever they are: events that
    /// do not match the filter, a different event under an id the client
    /// already holds, an event whose signature does not verify.
    #[must_use]
    pub fn serve(events: Vec<Event>) -> Self {
        Self::new().then_events(events).then_eose()
    }

    /// `CLOSED` and nothing else.
    #[must_use]
    pub fn closed(message: impl Into<String>) -> Self {
        Self::new().then_closed(message)
    }

    /// A NIP-42 challenge and nothing else -- as a reply to a REQ, this is
    /// the mid-subscription challenge rather than the connect-time one.
    #[must_use]
    pub fn auth(challenge: impl Into<String>) -> Self {
        Self::new().then_auth(challenge)
    }

    // ---- write-side whole behaviours ----------------------------------

    /// Store it and acknowledge it. What an unscripted EVENT gets.
    #[must_use]
    pub fn ok() -> Self {
        Self::new().then(Step::Ingest).then_ok("")
    }

    /// Acknowledge it with a message a real relay would send alongside a
    /// `true`, and store it.
    #[must_use]
    pub fn ok_with(message: impl Into<String>) -> Self {
        Self::new().then(Step::Ingest).then_ok(message)
    }

    /// `OK: true`, and the event is never stored -- so a subsequent REQ that
    /// matches it serves nothing. The relay said yes and kept nothing.
    #[must_use]
    pub fn ok_but_forget() -> Self {
        Self::new().then_ok("")
    }

    /// `OK: false` with the relay's own words, prefix included, because that
    /// is the string an app sees: `"duplicate: have this event"`,
    /// `"rate-limited: slow down"`, `"blocked: not admitted"`.
    #[must_use]
    pub fn rejected(message: impl Into<String>) -> Self {
        Self::new().then(Step::Ok {
            accepted: false,
            message: message.into(),
        })
    }

    // ---- composition ---------------------------------------------------

    #[must_use]
    pub fn then(mut self, step: Step) -> Self {
        self.steps.push(step);
        self
    }

    #[must_use]
    pub fn then_stored(self) -> Self {
        self.then(Step::Stored(Serve::Everything))
    }

    #[must_use]
    pub fn then_events(self, events: Vec<Event>) -> Self {
        self.then(Step::Events(events))
    }

    #[must_use]
    pub fn then_events_json(self, events: Vec<serde_json::Value>) -> Self {
        self.then(Step::EventsJson(events))
    }

    #[must_use]
    pub fn then_eose(self) -> Self {
        self.then(Step::Eose)
    }

    #[must_use]
    pub fn then_closed(self, message: impl Into<String>) -> Self {
        self.then(Step::Closed(message.into()))
    }

    #[must_use]
    pub fn then_notice(self, message: impl Into<String>) -> Self {
        self.then(Step::Notice(message.into()))
    }

    #[must_use]
    pub fn then_auth(self, challenge: impl Into<String>) -> Self {
        self.then(Step::Auth(challenge.into()))
    }

    #[must_use]
    pub fn then_ok(self, message: impl Into<String>) -> Self {
        self.then(Step::Ok {
            accepted: true,
            message: message.into(),
        })
    }

    /// Wait before whatever comes next.
    #[must_use]
    pub fn after(self, delay: Duration) -> Self {
        self.then(Step::Delay(delay))
    }

    #[must_use]
    pub fn then_stall(self) -> Self {
        self.then(Step::Stall)
    }

    #[must_use]
    pub fn then_disconnect(self) -> Self {
        self.then(Step::Disconnect)
    }

    #[must_use]
    pub fn then_bytes(self, bytes: impl Into<Vec<u8>>) -> Self {
        self.then(Step::Bytes(bytes.into()))
    }

    #[must_use]
    pub fn then_partial_frame(self, payload: impl Into<String>, keep_bytes: usize) -> Self {
        self.then(Step::PartialFrame {
            payload: payload.into(),
            keep_bytes,
        })
    }

    /// Cut a real `EVENT` frame for `event` after `keep_bytes` octets.
    #[must_use]
    pub fn then_partial_event(self, event: Event, keep_bytes: usize) -> Self {
        self.then(Step::PartialEvent {
            event: Box::new(event),
            keep_bytes,
        })
    }
}

/// What this relay does with the websocket upgrade request itself.
#[derive(Debug, Clone)]
pub enum Upgrade {
    /// Complete the handshake. The ordinary relay.
    Accept,
    /// Answer the upgrade with an ordinary HTTP response instead -- a captive
    /// portal's login page is `Http { status: 200, .. }` with an HTML body,
    /// and is the case a client that only handles a clean refusal gets wrong.
    Http {
        status: u16,
        content_type: String,
        body: String,
    },
    /// Accept the TCP connection and never answer at all.
    Hang,
}

/// What this relay publishes at its NIP-11 document address.
#[derive(Debug, Clone)]
pub enum Nip11 {
    /// No document: an HTTP fetch is answered `404`. Not a degenerate case --
    /// public relays that publish nothing are ordinary.
    None,
    /// This exact JSON body, so a scenario states the wire shape it means.
    Document(String),
}

impl Nip11 {
    /// A document advertising these `limitation` fields. Each is optional
    /// because NIP-11 omission means "said nothing", never an implicit zero.
    #[must_use]
    pub fn limits(max_subscriptions: Option<u64>, max_subid_length: Option<u64>) -> Self {
        let mut limitation = Vec::new();
        if let Some(value) = max_subscriptions {
            limitation.push(format!("\"max_subscriptions\":{value}"));
        }
        if let Some(value) = max_subid_length {
            limitation.push(format!("\"max_subid_length\":{value}"));
        }
        Self::Document(format!(
            "{{\"name\":\"relay-lab\",\"supported_nips\":[1,11],\"limitation\":{{{}}}}}",
            limitation.join(",")
        ))
    }
}

pub(crate) struct ReqRule {
    pub(crate) when: Req,
    pub(crate) only_nth: Option<usize>,
    pub(crate) then: Reply,
}

pub(crate) struct EventRule {
    pub(crate) when: Ev,
    pub(crate) then: Reply,
}

/// Everything one scripted relay does.
///
/// Unscripted is honest: it serves what it holds, acknowledges what it is
/// given, publishes no NIP-11 document, and verifies every inbound event's id
/// and signature.
pub struct Script {
    pub(crate) corpus: Vec<Event>,
    pub(crate) req_rules: Vec<ReqRule>,
    pub(crate) event_rules: Vec<EventRule>,
    pub(crate) auth_reply: Option<Reply>,
    pub(crate) on_connect: Option<Reply>,
    pub(crate) nip11: Nip11,
    pub(crate) upgrade: Upgrade,
    pub(crate) subscription_cap: Option<(usize, String)>,
    pub(crate) verify_writes: bool,
}

impl Default for Script {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Script {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Script")
            .field("corpus", &self.corpus.len())
            .field("req_rules", &self.req_rules.len())
            .field("event_rules", &self.event_rules.len())
            .field("nip11", &self.nip11)
            .field("upgrade", &self.upgrade)
            .field("subscription_cap", &self.subscription_cap)
            .field("verify_writes", &self.verify_writes)
            .finish()
    }
}

impl Script {
    #[must_use]
    pub fn new() -> Self {
        Self {
            corpus: Vec::new(),
            req_rules: Vec::new(),
            event_rules: Vec::new(),
            auth_reply: None,
            on_connect: None,
            nip11: Nip11::None,
            upgrade: Upgrade::Accept,
            subscription_cap: None,
            verify_writes: true,
        }
    }

    /// Pre-existing protocol state: what this relay already holds. Additive.
    #[must_use]
    pub fn seed(mut self, events: impl IntoIterator<Item = Event>) -> Self {
        self.corpus.extend(events);
        self
    }

    /// Run `then` for every REQ matching `when`. The first rule that matches
    /// wins; a REQ no rule matches gets [`Reply::stored`].
    #[must_use]
    pub fn on_req(mut self, when: Req, then: Reply) -> Self {
        self.req_rules.push(ReqRule {
            when,
            only_nth: None,
            then,
        });
        self
    }

    /// Run `then` only for the `n`th (1-based) REQ matching `when` on each
    /// connection. Later matches fall through to the next rule, so
    /// "misbehave once, then behave" is two lines.
    #[must_use]
    pub fn on_nth_req(mut self, n: usize, when: Req, then: Reply) -> Self {
        self.req_rules.push(ReqRule {
            when,
            only_nth: Some(n),
            then,
        });
        self
    }

    /// Run `then` for every inbound EVENT matching `when`. An EVENT no rule
    /// matches gets [`Reply::ok`].
    #[must_use]
    pub fn on_event(mut self, when: Ev, then: Reply) -> Self {
        self.event_rules.push(EventRule { when, then });
        self
    }

    /// Run `then` for an inbound NIP-42 `["AUTH", <event>]`. Default is
    /// `OK: true`.
    #[must_use]
    pub fn on_auth(mut self, then: Reply) -> Self {
        self.auth_reply = Some(then);
        self
    }

    /// Run `then` the moment the handshake completes, before the client has
    /// said anything -- a connect-time NIP-42 challenge, a NOTICE, a stall.
    #[must_use]
    pub fn on_connect(mut self, then: Reply) -> Self {
        self.on_connect = Some(then);
        self
    }

    #[must_use]
    pub fn nip11(mut self, document: Nip11) -> Self {
        self.nip11 = document;
        self
    }

    #[must_use]
    pub fn upgrade(mut self, policy: Upgrade) -> Self {
        self.upgrade = policy;
        self
    }

    /// Serve at most `n` simultaneous subscriptions per connection and
    /// `CLOSED` every REQ beyond that, without advertising the limit
    /// anywhere. Pair it with [`Nip11::limits`] for the advertised case, or
    /// leave the document silent for the case a client cannot see coming.
    #[must_use]
    pub fn cap_subscriptions(mut self, n: usize, message: impl Into<String>) -> Self {
        self.subscription_cap = Some((n, message.into()));
        self
    }

    /// Stop checking inbound events' id and signature.
    ///
    /// On by default, and left on for anything claiming to be realistic: a
    /// relay that admits an unsigned event is not a relay any client has to
    /// survive. This exists for scenarios about a client that sends something
    /// malformed on purpose and needs the relay to be the lax one.
    #[must_use]
    pub fn accepts_unverified_writes(mut self) -> Self {
        self.verify_writes = false;
        self
    }
}

/// Bodies `nostr::Event` will not hold, for the two dishonesty scenarios that
/// need them.
pub mod forge {
    use nostr::{Event, JsonUtil};

    /// The same event id, a different body. Serving this to a client that
    /// already holds the real one is the forgery case: an id is a commitment
    /// to content, and a relay is free to break it.
    #[must_use]
    pub fn different_body_same_id(original: &Event, content: &str) -> serde_json::Value {
        let mut value: serde_json::Value =
            serde_json::from_str(&original.as_json()).expect("an event always renders as JSON");
        value["content"] = serde_json::Value::String(content.to_string());
        value
    }

    /// The same event with its signature replaced by a syntactically valid
    /// one that does not verify.
    #[must_use]
    pub fn bad_signature(original: &Event) -> serde_json::Value {
        let mut value: serde_json::Value =
            serde_json::from_str(&original.as_json()).expect("an event always renders as JSON");
        value["sig"] = serde_json::Value::String("00".repeat(64));
        value
    }
}
