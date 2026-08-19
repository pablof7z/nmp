//! Drives NMP against scripted relays and prints what crossed the socket.
//!
//! A BINARY and not a test suite, and that is load-bearing here for the same
//! reasons `nmp-canary` gives and one of its own:
//!
//! - a restart is only a restart if the serving process exited, and the
//!   `external-*` scenarios kill one and start another on its store;
//! - a scenario here is a claim about OCTETS, and printing what the wire
//!   recorder actually saw beside the claim is worth more to a reader than a
//!   green dot. Every check prints its evidence whether it passed or not,
//!   because a number only shown on failure cannot be sanity-checked while it
//!   is still wrong but inside the assertion.
//!
//! Run:
//!
//! ```text
//! cargo run -p nmp-relay-lab --bin lab                  # every scenario
//! cargo run -p nmp-relay-lab --bin lab -- truncation    # one of them
//! cargo run -p nmp-relay-lab --bin lab -- list
//! cargo run -p nmp-relay-lab --bin lab -- mutate        # prove they can fail
//! ```
//!
//! `mutate` is the falsification mode, and it exists because a scenario that
//! cannot fail is worse than no scenario. Each one declares the deliberate
//! weakenings of its own script that MUST break it; `mutate` applies each and
//! requires a RED report. A mutation that leaves the scenario green is
//! reported as a failure of the scenario, not of the mutation -- which is how
//! the mid-frame truncation case was caught being structurally immune, its
//! truncated frame naming a subscription the client had never opened.
//!
//! Add `--features external-relay` for the two scenarios that need a real
//! third-party relay binary.

use std::time::Instant;

use nmp_relay_lab::scenario::Report;
use nmp_relay_lab::scenarios;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let selector = args.first().map(String::as_str).unwrap_or("all");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("the scenario runtime starts");

    let exit = match selector {
        "list" => {
            list();
            0
        }
        "mutate" => runtime.block_on(mutate()),
        "all" => runtime.block_on(run_all(None)),
        name => runtime.block_on(run_all(Some(name))),
    };
    std::process::exit(exit);
}

fn list() {
    println!("scenarios:");
    for scenario in scenarios::all() {
        let mutations = if scenario.mutations.is_empty() {
            String::new()
        } else {
            format!("   [mutations: {}]", scenario.mutations.join(", "))
        };
        println!("  {:<32} {}{mutations}", scenario.name, scenario.about);
    }
}

async fn run_all(only: Option<&str>) -> i32 {
    let scenarios = scenarios::all();
    if let Some(name) = only {
        if !scenarios.iter().any(|s| s.name == name) {
            eprintln!("no scenario named {name:?}. `lab list` shows them all.");
            return 2;
        }
    }

    let started = Instant::now();
    let mut reports: Vec<Report> = Vec::new();
    for scenario in scenarios {
        if only.is_some_and(|name| name != scenario.name) {
            continue;
        }
        println!("\n── {} ──────────────────────────────", scenario.name);
        println!("   {}", scenario.about);
        let report = (scenario.run)(None).await;
        report.print();
        reports.push(report);
    }

    let failed: Vec<&Report> = reports.iter().filter(|r| !r.passed()).collect();
    let skipped = reports.iter().filter(|r| r.skipped.is_some()).count();
    let checks: usize = reports.iter().map(|r| r.checks.len()).sum();

    println!("\n══════════════════════════════════════");
    println!(
        "{} scenarios, {checks} checks, {} failed, {skipped} skipped, in {:?}",
        reports.len(),
        failed.iter().filter(|r| r.skipped.is_none()).count(),
        started.elapsed()
    );
    for report in &failed {
        if report.skipped.is_none() {
            println!("  FAILED {} ({} checks)", report.name, report.failures());
        }
    }
    i32::from(failed.iter().any(|r| r.skipped.is_none()))
}

/// Prove every scenario can fail.
///
/// A mutation is applied to the scenario's own script and the report is
/// required to go RED. Green means the scenario was not measuring what its
/// name says -- so this reports it as a failure OF THE SCENARIO.
async fn mutate() -> i32 {
    let started = Instant::now();
    let mut applied = 0usize;
    let mut immune: Vec<String> = Vec::new();

    println!("── mutations ───────────────────────────");
    println!("   each must make its scenario go RED\n");
    for scenario in scenarios::all() {
        for mutation in scenario.mutations {
            applied += 1;
            let report = (scenario.run)(Some(mutation)).await;
            if report.skipped.is_some() {
                println!("  ~ {}/{mutation} skipped", scenario.name);
                applied -= 1;
                continue;
            }
            let reddened = !report.passed();
            let mark = if reddened { "  ✓" } else { "  ✗" };
            println!(
                "{mark} {}/{mutation} — {}",
                scenario.name,
                if reddened {
                    format!("{} check(s) went red", report.failures())
                } else {
                    "STILL GREEN: this scenario cannot detect its own mutation".to_string()
                }
            );
            if !reddened {
                immune.push(format!("{}/{mutation}", scenario.name));
            }
        }
    }

    println!("\n══════════════════════════════════════");
    println!(
        "{applied} mutations applied, {} left the scenario green, in {:?}",
        immune.len(),
        started.elapsed()
    );
    for name in &immune {
        println!("  IMMUNE {name}");
    }
    i32::from(!immune.is_empty())
}
