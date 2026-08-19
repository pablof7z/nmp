//! The scenarios that only exist across a process boundary.
//!
//! A restart is only a restart if the writing process actually exited. A second
//! `Engine` over the same store in one address space still holds the redb
//! pages, the allocator arenas and every decoded row, so a read served from
//! anywhere other than the durable file looks identical to a correct one. The
//! same goes for a crash: `Engine::shutdown` is a graceful path with its own
//! flush, and skipping it in-process still runs `Drop`.
//!
//! So this module holds the pieces both sides of a real fork need -- a durable
//! engine, a line protocol a child prints and a supervisor parses, and the
//! process-level survey (descriptors, threads, resident size) that is a
//! property of a process and not of a function.
//!
//! ## What the surface gives you here, and what it does not
//!
//! Good, and measured by the `restart` and `crash` scenarios: `publish_queue`
//! enumerates every retained obligation from a cold process with no in-memory
//! state, `reattach_receipt` reattaches to durable facts by stable id, and
//! `ReceiptId` is `pub struct ReceiptId(pub u64)` -- so an app persists `id.0`
//! and reconstructs `ReceiptId(value)` across a process boundary with no
//! ceremony at all. [`Handoff`] carries both receipt and event ids to prove it.
//! That is the whole recovery story and it survives SIGKILL intact.
//!
//! Fighting:
//!
//! - **`Engine::shutdown` returns `()`.** "Teardown finished" and "teardown
//!   finished cleanly" are the same observation, so [`timed_shutdown`] measures
//!   the wall clock and nothing else can be asserted.
//! - **Cross-process store exclusion is undocumented at the facade.**
//!   `EngineError::StoreAlreadyOpen` and `StoreStillOpen` exist, and
//!   `reset_persistent_store`'s doc says it "refuses a live in-process engine
//!   using the same canonical path; cross-process exclusion remains a separate
//!   deployment concern." What a second PROCESS gets is measured by the
//!   `contend` scenario rather than asserted here.

use std::time::{Duration, Instant};

use nmp::{Engine, EngineConfig, EventId, PublicKey, ReceiptId};

/// Open a durable engine on a real path, with this app's full capability set.
pub fn durable(store_path: &str) -> Result<crate::Canary, nmp::EngineError> {
    crate::Canary::open(Some(store_path.to_string()), Vec::new())
}

/// The line protocol between a child and its supervisor.
///
/// Deliberately dumb: one `key=value` per line on stdout, so a supervisor
/// reads it with `split_once('=')` and a human reads it by looking.
#[derive(Debug, Clone, Default)]
pub struct Handoff {
    pub author: Option<PublicKey>,
    pub events: Vec<EventId>,
    /// Stable receipt ids, carried across the process boundary as plain
    /// integers. `ReceiptId`'s field is public, so this round trip needs no
    /// accessor, no codec and no lookup.
    pub receipts: Vec<ReceiptId>,
}

impl Handoff {
    pub fn print(&self) {
        if let Some(author) = self.author {
            println!("handoff.author={}", author.to_hex());
        }
        for event in &self.events {
            println!("handoff.event={}", event.to_hex());
        }
        for receipt in &self.receipts {
            println!("handoff.receipt={}", receipt.0);
        }
        println!("handoff.count={}", self.events.len());
    }

    #[must_use]
    pub fn parse(stdout: &str) -> Self {
        let mut handoff = Self::default();
        for line in stdout.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "handoff.author" => handoff.author = PublicKey::from_hex(value.trim()).ok(),
                "handoff.event" => {
                    if let Ok(id) = EventId::from_hex(value.trim()) {
                        handoff.events.push(id);
                    }
                }
                "handoff.receipt" => {
                    if let Ok(raw) = value.trim().parse::<u64>() {
                        handoff.receipts.push(ReceiptId(raw));
                    }
                }
                _ => {}
            }
        }
        handoff
    }
}

/// Everything one retained write still says about itself after a cold start.
#[derive(Debug, Clone)]
pub struct Recovered {
    pub receipt: ReceiptId,
    pub event: EventId,
    pub author: PublicKey,
    pub signing: nmp::SigningState,
    pub route_complete: bool,
    pub intended: usize,
    pub outcome: Option<nmp::WriteOutcome>,
    /// Whether `reattach_receipt` found durable facts for it. The three
    /// outcomes (`Attached`, `NotFound`, `RetainedButUnreadable`) are distinct
    /// and this keeps them distinct.
    pub reattached: &'static str,
}

/// Enumerate every obligation the store retains, with no prior in-memory state.
///
/// This is the recovery door working: a cold process asks the store what it
/// owes and gets complete entries back, paged by a stable receipt-id cursor.
pub fn survey_publish_queue(engine: &Engine) -> Vec<Recovered> {
    let mut out = Vec::new();
    let mut after: Option<ReceiptId> = None;
    while let Ok(page) = engine.publish_queue(after, 64) {
        if page.is_empty() {
            break;
        }
        for entry in &page {
            let reattached = match engine.reattach_receipt(entry.receipt_id) {
                Ok(nmp::ReceiptReattachment::Attached { .. }) => "attached",
                Ok(nmp::ReceiptReattachment::NotFound) => "not-found",
                Ok(nmp::ReceiptReattachment::RetainedButUnreadable) => "unreadable",
                Err(_) => "engine-closed",
            };
            out.push(Recovered {
                receipt: entry.receipt_id,
                event: entry.event_id,
                author: entry.pubkey,
                signing: entry.signing.clone(),
                route_complete: entry.route_complete,
                intended: entry.relays.len(),
                outcome: entry.outcome.clone(),
                reattached,
            });
        }
        after = page.last().map(|entry| entry.receipt_id);
    }
    out
}

/// Shut the engine down and report how long it took to RETURN.
///
/// "The process is gone" and "teardown returned" are different signals and only
/// the second one is evidence that nothing was abandoned. `shutdown` returns
/// `()`, so the duration is the entire observation available.
pub fn timed_shutdown(engine: &Engine) -> Duration {
    let started = Instant::now();
    engine.shutdown();
    started.elapsed()
}

/// Process-level facts. Properties of a process, measurable only in one.
#[derive(Debug, Clone, Default)]
pub struct Survey {
    pub open_descriptors: Option<usize>,
    pub resident_kib: Option<u64>,
    /// NMP's own instrumented census. Doc-hidden, monotonic.
    pub nmp_threads_spawned: u64,
    /// NMP's own live gauge. Note what neither counts: threads the APP spawned
    /// because of NMP's delivery shape, such as `people::FollowButton`'s.
    pub nmp_threads_live: u64,
}

impl Survey {
    #[must_use]
    pub fn take() -> Self {
        Self {
            open_descriptors: open_descriptors(),
            resident_kib: resident_kib(),
            nmp_threads_spawned: nmp::nmp_threads_spawned(),
            nmp_threads_live: nmp::nmp_threads_live(),
        }
    }

    #[must_use]
    pub fn delta_descriptors(&self, before: &Survey) -> Option<i64> {
        Some(self.open_descriptors? as i64 - before.open_descriptors? as i64)
    }
}

impl std::fmt::Display for Survey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "fds={:?} rss_kib={:?} nmp_threads_spawned={} nmp_threads_live={}",
            self.open_descriptors,
            self.resident_kib,
            self.nmp_threads_spawned,
            self.nmp_threads_live
        )
    }
}

/// Open file descriptors, counted from `/dev/fd`. Portable enough for macOS and
/// Linux; `None` where the directory is not readable.
fn open_descriptors() -> Option<usize> {
    let entries = std::fs::read_dir("/dev/fd").ok()?;
    // The `read_dir` handle is itself an open descriptor, so subtract it.
    Some(entries.count().saturating_sub(1))
}

/// Resident set size in KiB, via `ps`. `None` when `ps` is unavailable or its
/// output does not parse.
fn resident_kib() -> Option<u64> {
    let pid = std::process::id();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Build an engine config for a durable store, spelled once.
#[must_use]
pub fn durable_config(store_path: &str) -> EngineConfig {
    EngineConfig {
        store_path: Some(store_path.to_string()),
        ..EngineConfig::default()
    }
}
