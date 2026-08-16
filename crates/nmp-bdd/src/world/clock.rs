//! The clock plane: what time this world's engine is running at.
//!
//! `features/writes/` is written in sentences about a stated instant --
//! *"Given my device clock reads 2026-07-29T12:00:00Z"*, *"And 2 seconds
//! later ..."*, *"Then the published event's created_at is ..."* -- and
//! `features/routing/` in sentences about time passing (*"And 30 days pass
//! with nothing learned"*). None of them are assertable against the real
//! system clock: an acceptance stamp is whatever the reducer's clock said,
//! and the reducer's clock is whatever the runtime last ticked it with.
//!
//! So this world STATES the time. `nmp::Engine::clock` (`#[doc(hidden)]`,
//! `unstable-mechanism`, the same hatch `mechanism_handle` uses) hands back
//! the one value every `Tick` the engine dispatches reads, and setting it
//! also delivers a tick -- so "30 days pass" is time the engine actually
//! acted on rather than a number nobody noticed.
//!
//! Its own module rather than a corner of `staging` because the stated time
//! has a LIFETIME of its own: it is chosen before the engine exists (a
//! `Given` runs before `ensure_started`), it must be re-applied to the fresh
//! engine a restart builds, and it outlives both. That is the same shape
//! `durable_store` has, and for the same reason.

use std::time::Duration;

use nostr::Timestamp;

use super::NmpWorld;

impl NmpWorld {
    /// `Given my device clock reads "<rfc3339>"`.
    ///
    /// Recorded even when nothing is started yet: [`Self::apply_clock`] is
    /// called from `spawn_engine`, so the instant is in force from the
    /// engine's first tick -- and again after a restart, which builds a
    /// brand-new engine whose clock starts unpinned.
    pub async fn set_device_clock(&mut self, rfc3339: &str) {
        let at = parse_stated_time(rfc3339);
        self.pinned_clock = Some(at);
        if self.started {
            self.apply_clock();
        }
    }

    /// `When 2 seconds later I ...` / `And 30 days pass`.
    ///
    /// Moves the STATED time, reading the real clock first if the scenario
    /// never stated one, so a time-travel step needs no preceding `Given`.
    pub async fn advance_clock(&mut self, by: Duration) {
        self.ensure_started().await;
        let clock = self.engine_clock();
        let next = clock.advance(by);
        self.pinned_clock = Some(next);
    }

    /// `When the engine is given a chance to act on the current time` -- the
    /// explicit drain. Delivers a tick carrying the time already in force,
    /// which is what lets a scenario say "and now everything due happens"
    /// without waiting on a wall-clock deadline to elapse on its own.
    pub async fn tick_engine(&mut self) {
        self.ensure_started().await;
        self.engine_clock().tick_now();
    }

    /// `When the engine ticks <n> times` / `And the publishing queue drains
    /// <n> times with nothing new learned`.
    ///
    /// The point of both sentences is that RE-RUNNING costs nothing, so the
    /// harness has to actually re-run it -- n times, deterministically,
    /// instead of waiting for a wall-clock deadline that may or may not fire
    /// inside a test's patience. Each tick goes through the same FIFO every
    /// command does, so when the last one returns the reducer has seen them
    /// all, and the settle window after it lets whatever they scheduled reach
    /// a relay before anything counts.
    pub async fn tick_engine_times(&mut self, times: usize) {
        self.ensure_started().await;
        let clock = self.engine_clock();
        for _ in 0..times {
            clock.tick_now();
        }
        self.wire_settled().await;
    }

    /// The instant this world has stated, if any -- what a `Then` compares a
    /// published `created_at` against when the scenario names no other.
    pub fn stated_clock(&self) -> Option<Timestamp> {
        self.pinned_clock
    }

    /// Put the stated instant in force on the engine that exists right now.
    /// Called by `staging::spawn_engine` on every construction, including the
    /// one a restart performs.
    pub(super) fn apply_clock(&mut self) {
        let Some(at) = self.pinned_clock else {
            return;
        };
        self.engine_clock().set(at);
    }

    fn engine_clock(&self) -> nmp_runtime::EngineClock {
        self.engine
            .as_ref()
            .expect("nmp-bdd: the engine must be started before its clock can be stated")
            .clock()
            .expect("nmp-bdd: a live engine always has a clock")
    }
}

/// `"2026-07-29T12:00:00Z"` -> a unix timestamp.
///
/// Hand-rolled rather than pulled in with a date library: the catalogue only
/// ever writes this one shape (UTC, whole seconds, `Z`), and a parser that
/// accepts exactly it turns a malformed literal in a `.feature` into a loud
/// failure instead of a silently reinterpreted one.
pub fn parse_stated_time(text: &str) -> Timestamp {
    let bytes = text.as_bytes();
    assert!(
        bytes.len() == 20
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[10] == b'T'
            && bytes[13] == b':'
            && bytes[16] == b':'
            && bytes[19] == b'Z',
        "nmp-bdd: a scenario states time as YYYY-MM-DDTHH:MM:SSZ, not {text:?}"
    );
    let field = |from: usize, to: usize| -> i64 {
        text[from..to]
            .parse()
            .unwrap_or_else(|_| panic!("nmp-bdd: {text:?} is not a well-formed timestamp"))
    };
    let (year, month, day) = (field(0, 4), field(5, 7), field(8, 10));
    let (hour, minute, second) = (field(11, 13), field(14, 16), field(17, 19));
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    assert!(
        secs > 0,
        "nmp-bdd: {text:?} is at or before the unix epoch, which the engine clock reserves"
    );
    Timestamp::from_secs(secs as u64)
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01 for a proleptic
/// Gregorian date. Exact for every date the catalogue can name, and short
/// enough to read in full rather than trust.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse, for an assertion that has to say what a stamped `created_at`
/// actually reads as when it is wrong.
pub fn format_stated_time(at: Timestamp) -> String {
    let secs = at.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}
