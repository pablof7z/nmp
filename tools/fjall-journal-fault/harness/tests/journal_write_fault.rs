//! The #818 regression: Fjall's journal `write_batch` result under a real
//! one-shot journal write failure.
//!
//! What this proves, and what it does not
//! --------------------------------------
//! Passing here qualifies exactly one behaviour of the pinned Fjall 3.1.8
//! build: an acknowledged transaction is not silently unrecoverable when the
//! journal write fails. It does not qualify Fjall's semantics, maintenance,
//! performance, or production readiness, and it does not select a database.
//! Redb remains NMP's production backend.
//!
//! Platform support
//! ----------------
//! The fault is a real `RLIMIT_FSIZE`/`SIGXFSZ` filesystem write failure. Linux
//! is the supported lane and executes the real fault. Other platforms are typed
//! as unsupported below rather than silently skipped -- the regression as a
//! whole is never skip-only.

use fjall_journal_fault_harness::{
    run, verify_pinned_identity, Evidence, Release, Role, MODES, RELEASES,
};

/// EFBIG. The journal write must fail for a real filesystem reason.
const EFBIG: u64 = 27;

struct Run {
    release: Release,
    evidence: Evidence,
}

fn run_matrix(mode: &str) -> Vec<Run> {
    let scratch = tempfile::tempdir().expect("probe scratch directory");
    RELEASES
        .iter()
        .map(|release| {
            let directory = scratch.path().join(format!("{}-{mode}", release.package));
            let evidence = run(release, mode, &directory)
                .unwrap_or_else(|error| panic!("fjall {} / {mode}: {error}", release.version));
            Run {
                release: *release,
                evidence,
            }
        })
        .collect()
}

fn find(runs: &[Run], role: Role) -> &Run {
    runs.iter()
        .find(|run| run.release.role == role)
        .expect("every role is present in the matrix")
}

/// Shared preconditions: the probe accepted its own injection, the fault landed
/// where it was aimed, and the journal did not rotate or compact underneath it.
fn assert_injection_is_sound(run: &Run, mode: &str) {
    let evidence = &run.evidence;
    assert!(
        evidence.refusals.is_empty(),
        "fjall {} / {mode}: probe refused the run: {:?}\n{}",
        run.release.version,
        evidence.refusals,
        evidence.raw
    );
    assert!(
        evidence.succeeded(),
        "fjall {} / {mode}: probe exited {:?}\n{}",
        run.release.version,
        evidence.exit_code,
        evidence.raw
    );
    assert_eq!(
        evidence.get("PROBE_VERSION"),
        run.release.version,
        "probe binary reports a different Fjall version than the package it was built from\n{}",
        evidence.raw
    );
    assert!(
        !evidence.flag("JOURNAL_ROTATED"),
        "fjall {} / {mode}: journal rotated, so the armed offset was not the target\n{}",
        run.release.version,
        evidence.raw
    );
}

#[test]
fn pinned_release_identities_match_the_recorded_evidence() {
    for release in &RELEASES {
        verify_pinned_identity(release)
            .unwrap_or_else(|error| panic!("fjall {}: {error}", release.version));
    }
}

/// Control: with no fault, every release commits and reopens to the same exact
/// post-state. A failure in the fault lanes is therefore attributable to the
/// injected fault, not to a broken fixture.
#[test]
fn healthy_control_commits_and_reopens_on_every_release() {
    if !supported_platform() {
        return unsupported_platform();
    }
    for run in &run_matrix("healthy") {
        assert_injection_is_sound(run, "healthy");
        let evidence = &run.evidence;
        assert_eq!(
            evidence.get("COMMIT_RESULT"),
            "ok",
            "fjall {}: healthy commit failed\n{}",
            run.release.version,
            evidence.raw
        );
        assert_eq!(
            evidence.number("SIGNAL_COUNT_TOTAL"),
            0,
            "fjall {}: healthy control must inject no fault\n{}",
            run.release.version,
            evidence.raw
        );
        let live = evidence.state("STATE_LIVE");
        assert_ne!(
            live,
            evidence.state("STATE_PRE"),
            "fjall {}: healthy control never applied the target transaction\n{}",
            run.release.version,
            evidence.raw
        );
        assert_eq!(
            live,
            evidence.state("STATE_REOPEN1"),
            "fjall {}: healthy commit did not survive reopen\n{}",
            run.release.version,
            evidence.raw
        );
        assert_eq!(
            evidence.state("STATE_REOPEN1"),
            evidence.state("STATE_REOPEN2"),
            "fjall {}: healthy state is not stable across two reopens\n{}",
            run.release.version,
            evidence.raw
        );
    }
}

/// The regression itself.
///
/// One real journal extension fails; the later `SyncAll` persistence path stays
/// healthy. 3.1.6 acknowledges a transaction it cannot recover; 3.1.7 and 3.1.8
/// return the journal error instead and leave no partial state anywhere.
#[test]
fn one_shot_journal_write_failure_separates_the_fixed_releases() {
    if !supported_platform() {
        return unsupported_platform();
    }
    let runs = run_matrix("one-shot");

    for run in &runs {
        assert_injection_is_sound(run, "one-shot");
        let evidence = &run.evidence;

        // Exactly one injected failure, and it landed inside the commit.
        assert_eq!(
            evidence.number("SIGNAL_COUNT_TOTAL"),
            1,
            "fjall {}: the fault must fire exactly once\n{}",
            run.release.version,
            evidence.raw
        );
        assert_eq!(
            evidence.number("SIGNAL_COUNT_DURING_COMMIT"),
            1,
            "fjall {}: the fault must fire during the target commit\n{}",
            run.release.version,
            evidence.raw
        );
        // Without this the batch never reaches the file descriptor inside
        // `write_batch`, and the run would be measuring the persist path.
        assert!(
            evidence.flag("TARGET_EXCEEDS_JOURNAL_BUFFER"),
            "fjall {}: target batch is not larger than the journal buffer, so \
             `write_batch` never reached the file descriptor\n{}",
            run.release.version,
            evidence.raw
        );

        // Whatever the release does in-process, the durable state after two
        // reopens is the exact pre-transaction state -- exact keys and values.
        for pass in ["STATE_REOPEN1", "STATE_REOPEN2"] {
            assert_eq!(
                evidence.state(pass),
                evidence.state("STATE_PRE"),
                "fjall {}: {pass} is not byte-identical to the pre-transaction state\n{}",
                run.release.version,
                evidence.raw
            );
        }
    }

    // --- 3.1.6: the acknowledged-loss counterexample ---------------------
    let negative = find(&runs, Role::NegativeControl);
    assert_eq!(
        negative.evidence.get("COMMIT_RESULT"),
        "ok",
        "fjall 3.1.6 was expected to acknowledge the failed journal write; if this now \
         returns an error, the negative control no longer demonstrates the defect\n{}",
        negative.evidence.raw
    );
    // The strongest actually observed unsafe shape: every batch key is visible
    // in-process after a commit whose journal record is truncated, and all of
    // it disappears on reopen.
    let live = negative.evidence.state("STATE_LIVE");
    let pre = negative.evidence.state("STATE_PRE");
    assert!(
        live.len() > pre.len(),
        "fjall 3.1.6: expected the acknowledged batch to be live in-process\n{}",
        negative.evidence.raw
    );
    assert!(
        pre.iter().all(|row| live.contains(row)),
        "fjall 3.1.6: live state lost pre-transaction rows\n{}",
        negative.evidence.raw
    );

    // --- 3.1.7 and 3.1.8: the journal error is returned -------------------
    for role in [Role::FixIntroduction, Role::Candidate] {
        let run = find(&runs, role);
        let evidence = &run.evidence;
        assert_eq!(
            evidence.get("COMMIT_RESULT"),
            "err",
            "fjall {}: the journal write error was not returned\n{}",
            run.release.version,
            evidence.raw
        );
        // `Io` and not `Poisoned`: the error came from `write_batch`, not from
        // the later persist path. That distinction is the whole regression.
        assert_eq!(
            evidence.get("COMMIT_ERROR_KIND"),
            "io",
            "fjall {}: expected the propagated journal IO error, not a persist-path error\n{}",
            run.release.version,
            evidence.raw
        );
        assert_eq!(
            evidence.number("COMMIT_ERROR_ERRNO"),
            EFBIG,
            "fjall {}: the returned error is not the injected filesystem failure\n{}",
            run.release.version,
            evidence.raw
        );
        // No partial transaction state through any affected keyspace.
        assert_eq!(
            evidence.state("STATE_LIVE"),
            evidence.state("STATE_PRE"),
            "fjall {}: partial transaction state is visible in-process\n{}",
            run.release.version,
            evidence.raw
        );
    }

    // --- the falsifier-of-the-falsifier -----------------------------------
    // If 3.1.7/3.1.8 were reverted to 3.1.6's `let _ = journal_writer.write_batch(..)`,
    // their commit result would match 3.1.6's and this fails.
    let candidate = find(&runs, Role::Candidate);
    assert_ne!(
        negative.evidence.get("COMMIT_RESULT"),
        candidate.evidence.get("COMMIT_RESULT"),
        "the candidate no longer differs from the 3.1.6 negative control, so the journal \
         error propagation has regressed\n{}\n{}",
        negative.evidence.raw,
        candidate.evidence.raw
    );
}

/// Control: a fault that stays armed through the persist path.
///
/// This is why the primary fault has to be one-shot. Under a persistent fault
/// 3.1.6 *also* returns an error -- `Poisoned`, from the later persist call --
/// so a test built on a persistent disk-full style fault would conclude that
/// 3.1.6 is safe.
#[test]
fn persistent_fault_cannot_stand_in_for_the_one_shot_result() {
    if !supported_platform() {
        return unsupported_platform();
    }
    let runs = run_matrix("persistent");
    for run in &runs {
        assert_injection_is_sound(run, "persistent");
        assert_eq!(
            run.evidence.get("COMMIT_RESULT"),
            "err",
            "fjall {}: a persistent fault must fail the commit\n{}",
            run.release.version,
            run.evidence.raw
        );
    }

    let negative = find(&runs, Role::NegativeControl);
    assert_eq!(
        negative.evidence.get("COMMIT_ERROR_KIND"),
        "poisoned",
        "fjall 3.1.6 under a persistent fault was expected to fail on the persist path; \
         without that, this control does not demonstrate the false green\n{}",
        negative.evidence.raw
    );
    for role in [Role::FixIntroduction, Role::Candidate] {
        let run = find(&runs, role);
        assert_eq!(
            run.evidence.get("COMMIT_ERROR_KIND"),
            "io",
            "fjall {}: expected the journal error to propagate before the persist path\n{}",
            run.release.version,
            run.evidence.raw
        );
    }
}

/// Control: a batch below the journal buffer never reaches the file descriptor
/// inside `write_batch`, so the fault lands on the later `persist` flush and
/// every release reports the same `Poisoned`. The harness must refuse to read
/// that as the regression passing.
#[test]
fn undersized_batch_is_refused_rather_than_silently_passing() {
    if !supported_platform() {
        return unsupported_platform();
    }
    let runs = run_matrix("undersized");
    for run in &runs {
        assert_injection_is_sound(run, "undersized");
        assert!(
            !run.evidence.flag("TARGET_EXCEEDS_JOURNAL_BUFFER"),
            "fjall {}: the undersized control is not actually below the journal buffer\n{}",
            run.release.version,
            run.evidence.raw
        );
    }

    // Indistinguishable outcomes: this mode carries no information about the
    // defect, which is exactly why the one-shot lane asserts the batch size.
    let negative = find(&runs, Role::NegativeControl);
    let candidate = find(&runs, Role::Candidate);
    assert_eq!(
        negative.evidence.get("COMMIT_ERROR_KIND"),
        candidate.evidence.get("COMMIT_ERROR_KIND"),
        "the undersized control was expected to be non-discriminating; if the releases now \
         differ here, the fault is no longer landing on the persist flush\n{}\n{}",
        negative.evidence.raw,
        candidate.evidence.raw
    );
    assert_eq!(
        negative.evidence.get("COMMIT_ERROR_KIND"),
        "poisoned",
        "fjall 3.1.6: the undersized fault was expected on the persist path\n{}",
        negative.evidence.raw
    );
}

/// Control: the fault is spent on a scratch file before the transaction, so the
/// journal write is healthy. No release may show the one-shot signature, and a
/// run like this must never be read as a pass.
#[test]
fn misinjected_fault_is_refused_rather_than_silently_passing() {
    if !supported_platform() {
        return unsupported_platform();
    }
    for run in &run_matrix("misinjected") {
        assert_injection_is_sound(run, "misinjected");
        let evidence = &run.evidence;
        assert_eq!(
            evidence.number("SIGNAL_COUNT_DURING_COMMIT"),
            0,
            "fjall {}: the mis-injection control leaked a fault into the commit\n{}",
            run.release.version,
            evidence.raw
        );
        assert_eq!(
            evidence.get("COMMIT_RESULT"),
            "ok",
            "fjall {}: a fault that never reached the journal must not fail the commit\n{}",
            run.release.version,
            evidence.raw
        );
        // The decisive check: the target transaction is durable. A mis-injected
        // run therefore cannot be mistaken for the 3.1.6 acknowledged-loss
        // shape, where the reopened state collapses back to the pre-state.
        assert_ne!(
            evidence.state("STATE_REOPEN1"),
            evidence.state("STATE_PRE"),
            "fjall {}: mis-injected run looks like the acknowledged-loss counterexample\n{}",
            run.release.version,
            evidence.raw
        );
    }
}

/// Every mode is exercised by a named test above. This keeps a newly added mode
/// from sitting unrun.
#[test]
fn every_probe_mode_is_covered_by_a_named_test() {
    let covered = [
        "healthy",
        "one-shot",
        "persistent",
        "undersized",
        "misinjected",
    ];
    for mode in MODES {
        assert!(
            covered.contains(&mode),
            "probe mode {mode} has no owning regression test"
        );
    }
}

fn supported_platform() -> bool {
    cfg!(target_os = "linux")
}

/// Typed, documented non-execution rather than a silent skip.
///
/// The fault depends on the kernel writing up to `RLIMIT_FSIZE` and then
/// failing the next `write(2)` with `EFBIG` while raising `SIGXFSZ` on the
/// writing thread. Linux is the lane that is verified to do this and it is the
/// lane CI runs; other kernels and filesystems are not claimed here.
fn unsupported_platform() {
    eprintln!(
        "fjall journal-write fault regression: unsupported platform ({}). The one-shot \
         RLIMIT_FSIZE/SIGXFSZ fault shape is only claimed for Linux; the supported CI lane \
         executes the real fault.",
        std::env::consts::OS
    );
}
