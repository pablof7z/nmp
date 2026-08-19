//! What a scenario reports, and how it can be shown to be capable of failing.
//!
//! This crate ships **scenarios in a binary**, not `#[test]`s. Two of the
//! reasons are the same ones `nmp-canary` gives -- a restart is only a restart
//! if the process exited, and descriptors are properties of a process -- and
//! one is specific to a relay harness: a scenario here is a claim about what
//! crossed a socket, and printing what the wire recorder actually saw beside
//! the claim is worth more to a reader than a green dot.
//!
//! # Positive controls and mutations
//!
//! A [`Report`] is a list of claims, each with the evidence that settled it.
//! Two disciplines are built into the shape rather than left to memory:
//!
//! - **Positive controls belong inside the scenario.** An assertion that
//!   nothing arrived is worthless beside a pipeline that delivered nothing at
//!   all, and the scenario that asserts it is the only place that can prove
//!   the difference. Several scenarios here serve one honest event alongside
//!   the dishonest ones for exactly this reason.
//! - **Mutations are a mode, not a memory.** Every scenario declares the
//!   deliberate weakenings of its own script that MUST make it fail
//!   ([`Scenario::mutations`]), and `lab mutate` runs each one and requires a
//!   red report. A mutation that leaves the scenario green is a scenario that
//!   was not testing what it said -- which is how the mid-frame truncation
//!   case was caught being structurally immune, its truncated frame naming a
//!   subscription the client had never opened.

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;

/// One settled claim, with the evidence that settled it.
#[derive(Debug, Clone)]
pub struct Check {
    pub claim: String,
    pub passed: bool,
    /// What was actually observed. Printed whether the check passed or not:
    /// a reader learning what the wire carried is the point, and a number
    /// only shown on failure cannot be sanity-checked when it is wrong but
    /// still inside the assertion.
    pub evidence: String,
}

/// Everything one scenario established.
#[derive(Debug, Clone)]
pub struct Report {
    pub name: String,
    pub checks: Vec<Check>,
    /// Set when the scenario could not run at all, as distinct from running
    /// and failing. A missing external binary is not a failed claim.
    pub skipped: Option<String>,
}

impl Report {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            checks: Vec::new(),
            skipped: None,
        }
    }

    /// Record a claim and the evidence for it.
    pub fn check(&mut self, claim: impl Into<String>, passed: bool, evidence: impl Into<String>) {
        self.checks.push(Check {
            claim: claim.into(),
            passed,
            evidence: evidence.into(),
        });
    }

    /// Record an equality claim, with both sides as the evidence.
    pub fn eq<T: Debug + PartialEq>(&mut self, claim: impl Into<String>, actual: T, expected: T) {
        let passed = actual == expected;
        let evidence = if passed {
            format!("{actual:?}")
        } else {
            format!("got {actual:?}, wanted {expected:?}")
        };
        self.check(claim, passed, evidence);
    }

    /// Record a claim whose evidence is a value that has to be shown either
    /// way -- a count, a list of subscription ids, a set of phases.
    pub fn that<T: Debug>(&mut self, claim: impl Into<String>, passed: bool, observed: T) {
        self.check(claim, passed, format!("{observed:?}"));
    }

    pub fn skip(&mut self, why: impl Into<String>) {
        self.skipped = Some(why.into());
    }

    #[must_use]
    pub fn passed(&self) -> bool {
        self.skipped.is_none() && !self.checks.is_empty() && self.checks.iter().all(|c| c.passed)
    }

    #[must_use]
    pub fn failures(&self) -> usize {
        self.checks.iter().filter(|c| !c.passed).count()
    }

    /// Print the scenario and every claim it settled.
    pub fn print(&self) {
        if let Some(why) = &self.skipped {
            println!("\n  {}  SKIPPED — {why}", self.name);
            return;
        }
        let verdict = if self.passed() { "ok" } else { "FAILED" };
        println!("\n  {} … {verdict}", self.name);
        for check in &self.checks {
            let mark = if check.passed { "  ✓" } else { "  ✗" };
            println!("{mark} {}", check.claim);
            println!("      {}", check.evidence);
        }
    }
}

/// A scenario body: takes the mutation to apply, if any, and settles a report.
pub type ScenarioBody = fn(Option<&'static str>) -> Pin<Box<dyn Future<Output = Report> + Send>>;

/// A scenario, and the deliberate weakenings that must break it.
pub struct Scenario {
    pub name: &'static str,
    /// One line saying what a reader is about to see proven.
    pub about: &'static str,
    /// Names of the mutations this scenario knows how to apply to its own
    /// script. `lab mutate` runs each and requires the report to go red.
    pub mutations: &'static [&'static str],
    pub run: ScenarioBody,
}

/// Build the `run` field from an async fn taking `Option<&'static str>`.
#[macro_export]
macro_rules! scenario_entry {
    ($name:literal, $about:literal, $mutations:expr, $body:path) => {
        $crate::scenario::Scenario {
            name: $name,
            about: $about,
            mutations: $mutations,
            run: |mutation| Box::pin($body(mutation)),
        }
    };
}
