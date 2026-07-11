//! Read-only bootstrap phase, run once BEFORE `EngineThread::spawn`.
//!
//! `nmp_router::RelayDirectory` is boxed into `EngineCore` exactly once at
//! construction and is never mutated afterward (see `directory.rs`'s doc).
//! To get real outbox routing for the follow-feed, this app therefore has
//! to resolve NIP-65 write relays for the target pubkey's follows *before*
//! the engine exists — there is no other point at which it could be fed in.
//!
//! This talks to the wire directly through `nmp_transport::Pool` (the same
//! crate `nmp-engine::runtime` itself drives) rather than hand-rolling a
//! websocket client — HARVEST, not scratch crypto/wire-format code:
//!
//! 1. REQ kind:3 (contact list) for the target pubkey, from both indexer
//!    relays. Whichever copy has the newest `created_at` wins.
//! 2. REQ kind:10002 (NIP-65 relay list) for the target + every follow, from
//!    the same two indexer relays. For each author, keep the newest event
//!    and take its `r` tags whose marker is absent or `"write"` as that
//!    author's write relays.
//!
//! Both phases are bounded: each runs until every relay handle has sent an
//! EOSE for that phase's subscription, or `phase_timeout` elapses, whichever
//! is first — this function always returns.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use nmp_router::{Lane, LanedRelay, PubkeyHex};
use nostr::SubscriptionId;
use nostr::{ClientMessage, Event, Filter, JsonUtil, Kind, PublicKey, RelayMessage, RelayUrl};

use nmp_transport::{Pool, PoolConfig, PoolEvent, RelayFrame, RelayHandle, WireFrame};

use crate::directory::BootstrapDirectory;

/// What the bootstrap phase actually observed — printed honestly rather
/// than assumed, since indexer relays are real infrastructure the demo does
/// not control.
#[derive(Debug, Default)]
pub struct BootstrapStats {
    pub contact_list_found: bool,
    pub follows_discovered: usize,
    pub relay_list_events_seen: usize,
    pub authors_with_write_relays: usize,
    pub total_write_relay_urls: usize,
    pub elapsed: Duration,
}

/// Cap on how many authors' NIP-65 lists we ask for in one filter, purely
/// so a target with an enormous follow list doesn't build a pathological
/// wire filter. Not an engine limit -- an app-level guard on OUR OWN
/// bootstrap fetch.
const MAX_RELAY_LIST_AUTHORS: usize = 400;

pub fn bootstrap(
    indexers: &[RelayUrl],
    target: PublicKey,
    phase_timeout: Duration,
) -> (BootstrapDirectory, BootstrapStats) {
    let start = Instant::now();
    let mut stats = BootstrapStats::default();

    let (evt_tx, evt_rx) = mpsc::channel::<PoolEvent>();
    let pool = Pool::new(PoolConfig::default(), evt_tx);

    let handles: Vec<RelayHandle> = indexers.iter().map(|url| pool.ensure_open(url)).collect();

    // ---- Phase 1: kind:3 contact list for `target` ----------------------
    let contacts_sub = SubscriptionId::new("nmp-demo-contacts");
    let contacts_filter = Filter::new().author(target).kind(Kind::ContactList);
    send_req(&pool, &handles, &contacts_sub, contacts_filter);

    let mut newest_contacts: Option<Event> = None;
    collect_events(
        &pool,
        &evt_rx,
        &handles,
        &contacts_sub,
        phase_timeout,
        |ev| {
            if ev.kind == Kind::ContactList
                && ev.pubkey == target
                && newest_contacts
                    .as_ref()
                    .is_none_or(|cur| ev.created_at > cur.created_at)
            {
                newest_contacts = Some(ev.clone());
            }
        },
    );
    send_close(&pool, &handles, &contacts_sub);

    stats.contact_list_found = newest_contacts.is_some();
    let follows: BTreeSet<PublicKey> = newest_contacts
        .as_ref()
        .map(|ev| {
            ev.tags
                .iter()
                .filter_map(|t| {
                    let s = t.as_slice();
                    (s.first().map(String::as_str) == Some("p"))
                        .then(|| s.get(1))
                        .flatten()
                        .and_then(|hex| PublicKey::from_hex(hex).ok())
                })
                .collect()
        })
        .unwrap_or_default();
    stats.follows_discovered = follows.len();

    // ---- Phase 2: kind:10002 (NIP-65) for target + every follow ----------
    let mut authors: Vec<PublicKey> = std::iter::once(target).chain(follows).collect();
    authors.truncate(MAX_RELAY_LIST_AUTHORS);

    let relaylist_sub = SubscriptionId::new("nmp-demo-relaylists");
    let relaylist_filter = Filter::new().authors(authors).kind(Kind::RelayList);
    send_req(&pool, &handles, &relaylist_sub, relaylist_filter);

    let mut newest_relaylist: HashMap<PubkeyHex, Event> = HashMap::new();
    collect_events(
        &pool,
        &evt_rx,
        &handles,
        &relaylist_sub,
        phase_timeout,
        |ev| {
            if ev.kind != Kind::RelayList {
                return;
            }
            stats.relay_list_events_seen += 1;
            let author = ev.pubkey.to_hex();
            let newer = newest_relaylist
                .get(&author)
                .is_none_or(|cur| ev.created_at > cur.created_at);
            if newer {
                newest_relaylist.insert(author, ev.clone());
            }
        },
    );
    send_close(&pool, &handles, &relaylist_sub);

    pool.shutdown();

    let mut write: HashMap<PubkeyHex, Vec<LanedRelay>> = HashMap::new();
    for (author, ev) in &newest_relaylist {
        let relays: Vec<LanedRelay> = ev
            .tags
            .iter()
            .filter_map(|t| {
                let s = t.as_slice();
                if s.first().map(String::as_str) != Some("r") {
                    return None;
                }
                let url = RelayUrl::parse(s.get(1)?).ok()?;
                // Absent marker == both read+write; explicit "read" is
                // read-only and excluded from the write set.
                match s.get(2).map(String::as_str) {
                    Some("read") => None,
                    _ => Some(LanedRelay::new(url, Lane::Nip65Write)),
                }
            })
            .collect();
        if !relays.is_empty() {
            write.insert(author.clone(), relays);
        }
    }
    stats.authors_with_write_relays = write.len();
    stats.total_write_relay_urls = write.values().map(Vec::len).sum();
    stats.elapsed = start.elapsed();

    (BootstrapDirectory::new(write, indexers.to_vec()), stats)
}

fn send_req(pool: &Pool, handles: &[RelayHandle], sub: &SubscriptionId, filter: Filter) {
    let text = ClientMessage::req(sub.clone(), vec![filter]).as_json();
    for h in handles {
        let _ = pool.send(*h, WireFrame::Text(text.clone()));
    }
}

fn send_close(pool: &Pool, handles: &[RelayHandle], sub: &SubscriptionId) {
    let text = ClientMessage::close(sub.clone()).as_json();
    for h in handles {
        let _ = pool.send(*h, WireFrame::Text(text.clone()));
    }
}

/// Drain `evt_rx` until every handle in `handles` has EOSE'd `sub`, or
/// `timeout` elapses -- whichever is first. Calls `on_event` for every
/// `EVENT` frame whose subscription id matches `sub`, regardless of EOSE
/// state (late-arriving stored events before EOSE are still valid).
fn collect_events(
    _pool: &Pool,
    evt_rx: &mpsc::Receiver<PoolEvent>,
    handles: &[RelayHandle],
    sub: &SubscriptionId,
    timeout: Duration,
    mut on_event: impl FnMut(&Event),
) {
    let deadline = Instant::now() + timeout;
    let mut eosed: HashSet<RelayHandle> = HashSet::new();
    let wanted: HashSet<RelayHandle> = handles.iter().copied().collect();

    loop {
        if eosed.len() >= wanted.len() {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        match evt_rx.recv_timeout(remaining) {
            Ok(PoolEvent::Frame {
                handle,
                frame: RelayFrame::Text(text),
            }) => {
                let Ok(msg) = RelayMessage::from_json(text.as_bytes()) else {
                    continue;
                };
                match msg {
                    RelayMessage::Event {
                        subscription_id,
                        event,
                    } if subscription_id.as_ref() == sub => {
                        on_event(event.as_ref());
                    }
                    RelayMessage::EndOfStoredEvents(subscription_id)
                        if subscription_id.as_ref() == sub =>
                    {
                        eosed.insert(handle);
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return,
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
