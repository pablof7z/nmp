//! The wire recorder: what each side actually put on the socket, decoded.
//!
//! A guarantee scenario must never take NMP's own report as its only witness.
//! If the engine's diagnostics claim one subscription and its coverage claims
//! a relay finished, a bug in either makes the guarantee un-falsifiable --
//! the scenario passes because the thing under test said so. So every
//! assertion about what NMP did on the wire reads this instead, and this is
//! independent of the engine by construction: it is the relay's own record of
//! the octets that reached it and the octets it sent back.
//!
//! Both directions, unlike the tap it replaces, which "never sees the
//! relay-to-client direction". A scenario that scripts a downstream frame can
//! therefore assert the frame was actually written, not merely queued.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Which way one frame went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// NMP wrote it.
    Up,
    /// The relay wrote it.
    Down,
}

/// One frame, as it crossed the socket.
#[derive(Debug, Clone)]
pub struct WireFrame {
    /// Which client connection. A second connection to the same relay is a
    /// reconnect, or a second engine (both are scenarios this crate exists
    /// for), and the two must never be conflated into one stream.
    pub connection: usize,
    pub direction: Direction,
    /// The decoded NIP-01 message, or `None` for octets that were not one --
    /// a deliberately truncated frame, injected bytes, a control frame.
    pub message: Option<serde_json::Value>,
    /// The literal payload, always.
    pub payload: Vec<u8>,
}

impl WireFrame {
    /// The NIP-01 verb (`REQ`, `EVENT`, `EOSE`, ...), if this frame is one.
    #[must_use]
    pub fn verb(&self) -> Option<&str> {
        self.message.as_ref()?.as_array()?.first()?.as_str()
    }

    fn arg_str(&self, index: usize) -> Option<&str> {
        self.message.as_ref()?.as_array()?.get(index)?.as_str()
    }
}

/// One REQ NMP put on the socket.
#[derive(Debug, Clone)]
pub struct WireReq {
    pub connection: usize,
    pub sub_id: String,
    /// The REQ's filters, verbatim.
    pub filters: Vec<serde_json::Value>,
    /// `true` iff `sub_id` was already live when this REQ arrived -- a NIP-01
    /// in-place filter REPLACEMENT rather than a newly opened subscription.
    pub replaces: bool,
}

impl WireReq {
    /// The largest `limit` any filter carries. An absent `limit` is unbounded
    /// and is deliberately not reported as zero: unbounded and "asks for
    /// nothing" are opposite conditions.
    #[must_use]
    pub fn max_limit(&self) -> Option<u64> {
        self.filters
            .iter()
            .filter_map(|f| f.get("limit"))
            .filter_map(serde_json::Value::as_u64)
            .max()
    }

    #[must_use]
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

    #[must_use]
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

    #[must_use]
    pub fn tag_values(&self, name: char) -> BTreeSet<String> {
        let key = format!("#{name}");
        self.filters
            .iter()
            .filter_map(|f| f.get(&key))
            .filter_map(serde_json::Value::as_array)
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect()
    }
}

/// Everything both sides sent, in arrival order.
#[derive(Debug, Clone, Default)]
pub struct WireRecord {
    pub frames: Vec<WireFrame>,
}

impl WireRecord {
    /// Every REQ NMP sent, with replacement already worked out per
    /// connection.
    #[must_use]
    pub fn reqs(&self) -> Vec<WireReq> {
        let mut live: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
        let mut reqs = Vec::new();
        for frame in &self.frames {
            if frame.direction != Direction::Up {
                continue;
            }
            let Some(array) = frame.message.as_ref().and_then(serde_json::Value::as_array) else {
                continue;
            };
            match array.first().and_then(serde_json::Value::as_str) {
                Some("REQ") => {
                    let Some(sub_id) = array.get(1).and_then(serde_json::Value::as_str) else {
                        continue;
                    };
                    let live = live.entry(frame.connection).or_default();
                    let replaces = live.contains(sub_id);
                    live.insert(sub_id.to_string());
                    reqs.push(WireReq {
                        connection: frame.connection,
                        sub_id: sub_id.to_string(),
                        filters: array[2..].to_vec(),
                        replaces,
                    });
                }
                Some("CLOSE") => {
                    if let Some(sub_id) = array.get(1).and_then(serde_json::Value::as_str) {
                        live.entry(frame.connection).or_default().remove(sub_id);
                    }
                }
                _ => {}
            }
        }
        reqs
    }

    /// Subscription ids NMP closed, in order.
    #[must_use]
    pub fn closes(&self) -> Vec<String> {
        self.up_verb("CLOSE")
            .filter_map(|frame| frame.arg_str(1).map(str::to_string))
            .collect()
    }

    /// Distinct subscription ids NMP opened, in first-seen order.
    #[must_use]
    pub fn subscription_ids(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut order = Vec::new();
        for req in self.reqs() {
            if seen.insert(req.sub_id.clone()) {
                order.push(req.sub_id);
            }
        }
        order
    }

    /// Subscription ids NMP opened and has not since closed.
    #[must_use]
    pub fn live_subscription_ids(&self) -> Vec<String> {
        let closes = self.closes();
        self.subscription_ids()
            .into_iter()
            .filter(|id| !closes.contains(id))
            .collect()
    }

    /// Event ids in every EVENT frame NMP sent -- writes, including attempts
    /// the relay refused before admitting anything.
    #[must_use]
    pub fn published_event_ids(&self) -> Vec<String> {
        self.up_verb("EVENT")
            .filter_map(|frame| {
                frame
                    .message
                    .as_ref()?
                    .as_array()?
                    .get(1)?
                    .get("id")?
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }

    /// The NIP-42 AUTH events NMP sent, whole.
    #[must_use]
    pub fn auth_responses(&self) -> Vec<serde_json::Value> {
        self.up_verb("AUTH")
            .filter_map(|frame| frame.message.as_ref()?.as_array()?.get(1).cloned())
            .collect()
    }

    /// Event ids in every EVENT frame the RELAY sent, in order -- what NMP
    /// was actually served, independent of what it did with them.
    #[must_use]
    pub fn served_event_ids(&self) -> Vec<String> {
        self.down_verb("EVENT")
            .filter_map(|frame| {
                frame
                    .message
                    .as_ref()?
                    .as_array()?
                    .get(2)?
                    .get("id")?
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }

    /// Subscription ids the relay sent EOSE for.
    #[must_use]
    pub fn eosed_subscription_ids(&self) -> Vec<String> {
        self.down_verb("EOSE")
            .filter_map(|frame| frame.arg_str(1).map(str::to_string))
            .collect()
    }

    /// `(sub-id, message)` for every CLOSED the relay sent.
    #[must_use]
    pub fn closed_sent(&self) -> Vec<(String, String)> {
        self.down_verb("CLOSED")
            .filter_map(|frame| {
                Some((
                    frame.arg_str(1)?.to_string(),
                    frame.arg_str(2).unwrap_or_default().to_string(),
                ))
            })
            .collect()
    }

    /// `(event-id, accepted, message)` for every OK the relay sent.
    #[must_use]
    pub fn oks_sent(&self) -> Vec<(String, bool, String)> {
        self.down_verb("OK")
            .filter_map(|frame| {
                let array = frame.message.as_ref()?.as_array()?;
                Some((
                    array.get(1)?.as_str()?.to_string(),
                    array.get(2)?.as_bool()?,
                    array.get(3).and_then(serde_json::Value::as_str).unwrap_or_default().to_string(),
                ))
            })
            .collect()
    }

    /// The challenges the relay put on the wire.
    #[must_use]
    pub fn auth_challenges(&self) -> Vec<String> {
        self.down_verb("AUTH")
            .filter_map(|frame| frame.arg_str(1).map(str::to_string))
            .collect()
    }

    /// How many distinct client connections this record covers. A second
    /// connection is a reconnect or a second engine.
    #[must_use]
    pub fn connections(&self) -> usize {
        self.frames
            .iter()
            .map(|frame| frame.connection)
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// Only the frames one connection carried.
    #[must_use]
    pub fn on_connection(&self, connection: usize) -> Self {
        Self {
            frames: self
                .frames
                .iter()
                .filter(|frame| frame.connection == connection)
                .cloned()
                .collect(),
        }
    }

    fn up_verb<'a>(&'a self, verb: &'a str) -> impl Iterator<Item = &'a WireFrame> + 'a {
        self.frames
            .iter()
            .filter(move |f| f.direction == Direction::Up && f.verb() == Some(verb))
    }

    fn down_verb<'a>(&'a self, verb: &'a str) -> impl Iterator<Item = &'a WireFrame> + 'a {
        self.frames
            .iter()
            .filter(move |f| f.direction == Direction::Down && f.verb() == Some(verb))
    }
}

#[derive(Debug, Default)]
struct Inner {
    frames: Vec<WireFrame>,
    connections: usize,
    /// Anything the decoder could not account for. Surfaced as a PANIC at
    /// read time rather than swallowed: a silently dropped frame would turn a
    /// red scenario green.
    faults: Vec<String>,
}

/// Shared, cloneable recorder.
#[derive(Debug, Clone, Default)]
pub struct WireLog {
    inner: Arc<Mutex<Inner>>,
    notify: Arc<tokio::sync::Notify>,
}

impl WireLog {
    pub(crate) fn next_connection(&self) -> usize {
        let mut inner = self.lock();
        let id = inner.connections;
        inner.connections += 1;
        id
    }

    pub(crate) fn record(&self, connection: usize, direction: Direction, payload: Vec<u8>) {
        let message = serde_json::from_slice::<serde_json::Value>(&payload).ok();
        self.lock().frames.push(WireFrame {
            connection,
            direction,
            message,
            payload,
        });
        self.notify.notify_waiters();
    }

    pub(crate) fn fault(&self, what: String) {
        self.lock().faults.push(what);
        self.notify.notify_waiters();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Everything recorded so far. Panics if the decoder ever failed to
    /// account for a frame: a count read off an incomplete record is worse
    /// than no count.
    #[must_use]
    pub fn snapshot(&self) -> WireRecord {
        let inner = self.lock();
        assert!(
            inner.faults.is_empty(),
            "nmp-relay-lab: the wire decoder could not account for every frame, \
             so no count read from it is trustworthy: {:?}",
            inner.faults
        );
        WireRecord {
            frames: inner.frames.clone(),
        }
    }

    fn frame_count(&self) -> usize {
        self.lock().frames.len()
    }

    /// Bounded wait for the record to satisfy `predicate`.
    ///
    /// Never a spin-poll. `Notified::enable()` registers the waiter before the
    /// second check, closing the lost-wakeup window a bare `notified()` leaves
    /// open: just pinning the future does not register it, so a frame landing
    /// between the check and the first poll would notify zero waiters.
    pub async fn wait_for(
        &self,
        timeout: Duration,
        predicate: impl Fn(&WireRecord) -> bool,
    ) -> bool {
        let wait = async {
            loop {
                if predicate(&self.snapshot()) {
                    return;
                }
                let notified = self.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if predicate(&self.snapshot()) {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(timeout, wait).await.is_ok()
    }

    /// Block until neither side has sent anything for a whole `quiet` window,
    /// or `max` elapses.
    ///
    /// Nearly every count-shaped assertion is only meaningful against a
    /// settled wire: NMP recompiles demand as rows arrive, so REQs keep
    /// coming while demand is still resolving. Coarse by design -- one sleep
    /// per window against a monotonic counter, never a spin-poll.
    pub async fn wait_quiet(&self, quiet: Duration, max: Duration) {
        let deadline = Instant::now() + max;
        loop {
            let before = self.frame_count();
            tokio::time::sleep(quiet).await;
            if self.frame_count() == before || Instant::now() >= deadline {
                return;
            }
        }
    }
}
