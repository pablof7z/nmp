//! #1182 falsifier 5: no file under `crates/nmp-nip29/src/` special-cases
//! publication visibility. Moved here from `crates/nmp/tests/` by #1707
//! alongside the code it scans -- NIP-29's app-facing door left
//! `crates/nmp/src/nip29/` entirely, so `crates/nmp-nip29/src/` is now the
//! whole of NIP-29's source, not one of two directories to scan.
//!
//! Whether a locally accepted write appears in a matching live query, and what
//! provenance it reports while no relay has carried it, is ordinary engine
//! behaviour that every protocol gets for free. NIP-29 is a vocabulary of
//! kinds and a per-relay authority rule; it has no opinion about the outbound
//! publication queue and must never acquire one.
//!
//! The gate is a source scan for the vocabulary such a special case would have
//! to be written in: reading local-write origin, or reaching into the store's
//! projection doors. If a branch ever appears there, the term it needs shows up
//! here first.
//!
//! Prose is exempt: `discovery.rs` legitimately EXPLAINS the per-relay
//! authority rule and points at the general engine mechanism by name. Only
//! code is scanned -- a documentation reference is the opposite of a special
//! case.
//!
//! # Labelling a row is not deciding whether it is seen (#1233)
//!
//! This gate used to ban the substring `.sources` outright, on the reasoning
//! that "branching on it is a visibility rule". Branching on it is. READING it
//! is not, and the two need separating, because NIP-29's per-relay authority
//! makes "which relay served this record" a question NIP-29 genuinely owns and
//! must answer: a group's metadata, admins and members are relay-signed, two
//! relays hosting one group id are two independent groups, and an aggregate
//! that could not say which relay supported which fact would be exactly the
//! confidently-wrong answer the whole design exists to prevent (#1233). The
//! blanket ban also swept up `AcquisitionEvidence::sources`, which is not
//! provenance at all -- it is the per-relay acquisition fact every availability
//! projection in the repository reads, `nmp_nip02`'s included.
//!
//! So the ban is now on the SHAPE a visibility rule must take rather than on
//! the noun it would read. A visibility rule has to reach a verdict: it tests
//! a row's relay set for emptiness or membership, or narrows it against some
//! other set, and then drops, hides or specially-cases the row. Labelling
//! never does any of that -- it carries the set through verbatim and attaches
//! it to what it describes. `check_no_verdicts_on_a_rows_relay_set` below
//! fails on the first shape and passes the second, and every original banned
//! identifier is still banned unchanged.

use std::path::{Path, PathBuf};

/// The vocabulary a publication-visibility special case cannot be written
/// without. Each entry is a thing NIP-29 code would have to TOUCH in order to
/// decide, for itself, whether a published event may be seen.
///
/// Deliberately NOT banned: `WriteFact`, `FifoReceiver`, `LiveQuery`,
/// `CacheMode`. Handing back the engine's ordinary receipt stream, and
/// stamping `CacheMode::Strict` for per-relay authority (#1173), are NIP-29
/// USING the general doors -- the opposite of special-casing them. Only
/// reading where a row came from, or reaching past the general projection into
/// the store's own, could implement a visibility rule of its own.
const BANNED_IDENTIFIERS: &[&str] = &[
    // Row/store provenance: "which relays carried this", "was this ours".
    "provenance",
    "visible_under_pin",
    "LocalOrigin",
    "local_origin",
    "SigState",
    "AcceptWrite",
    "accept_write",
    // The store's own pinned projection doors, bypassing the general one.
    "query_newest_under_pin",
    "query_newest_before_under_pin",
    "query_newest_before_any_under_pin",
];

/// The operators a verdict on a row's relay set has to be written with. A
/// visibility rule cannot avoid one of these: it must ask whether the set is
/// empty, whether it contains something, or what it has in common with
/// something else, and then act on the answer.
///
/// Deliberately NOT here: plain reads (`for host in attributed`,
/// `hosts: sources`, `*attributed = sources`). Those carry the engine's own
/// answer through unchanged and attach it to the record it describes, which is
/// the per-relay attribution NIP-29 owns and the read-side twin of the
/// `CacheMode::Strict` its demands already carry.
const BANNED_RELAY_SET_VERDICTS: &[&str] = &[
    "is_empty",
    "contains",
    "intersection",
    "difference",
    "retain",
    "filter",
    "any(",
    "all(",
];

/// The ways NIP-29 code can name a row's relay set.
const ROW_RELAY_SET: &[&str] = &["sources", "attributed"];

#[test]
fn nip29_code_never_names_publication_visibility_vocabulary() {
    let mut violations = Vec::new();
    for dir in nip29_dirs() {
        scan(&dir, &mut violations);
    }
    check_no_verdicts_on_a_rows_relay_set(&mut violations);
    assert!(
        !violations.is_empty() || !nip29_dirs().is_empty(),
        "the scan must actually have directories to look at"
    );
    assert!(
        violations.is_empty(),
        "NIP-29 must contain NO special case for publication visibility -- that \
         rule is general engine behaviour every protocol gets for free (#1182). \
         Offending line(s):\n{}",
        violations.join("\n")
    );
}

/// A row's relay set may be read and carried, never reduced to a verdict.
///
/// The line-level rule is deliberately blunt: naming a row's relay set on the
/// same line as an emptiness/membership/set-narrowing operator is refused,
/// whatever the intent. `if sources.is_empty() { continue }` -- the shape of
/// "hide the write I have not sent yet" -- cannot be written; nor can
/// `&sources & hosts`, which narrows a row's own answer to a set NIP-29 chose
/// and which this gate caught in the first draft of the group-records reader.
///
/// `AcquisitionEvidence`'s own per-relay facts are a different thing entirely
/// and are reached through `branch.sources`, which names a BRANCH and not a
/// row; the availability ladder that reads them is exempt for that reason and
/// is required to say so by naming its accessor `branch`.
fn check_no_verdicts_on_a_rows_relay_set(violations: &mut Vec<String>) {
    for dir in nip29_dirs() {
        for path in rust_files(&dir) {
            let content = std::fs::read_to_string(&path).expect("source file must be readable");
            for (lineno, line) in content.lines().enumerate() {
                let Some(code) = code_of(line) else { continue };
                // A branch's acquisition facts are not a row's provenance.
                if code.contains("branch.sources") {
                    continue;
                }
                let names_a_rows_relay_set = ROW_RELAY_SET.iter().any(|term| code.contains(term));
                if !names_a_rows_relay_set {
                    continue;
                }
                for verdict in BANNED_RELAY_SET_VERDICTS {
                    if code.contains(verdict) {
                        violations.push(format!(
                            "{}:{}: a verdict ({verdict:?}) on a row's own relay set in NIP-29                              code -- read it and carry it, never reduce it to a decision: {}",
                            path.display(),
                            lineno + 1,
                            line.trim()
                        ));
                    }
                }
            }
        }
    }
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("a NIP-29 source dir must be readable") {
        let path = entry.expect("dir entry must be readable").path();
        if path.is_dir() {
            found.extend(rust_files(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            found.push(path);
        }
    }
    found
}

fn nip29_dirs() -> Vec<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/nmp-nip29 has a parent")
        .parent()
        .expect("crates has a parent (workspace root)")
        .to_path_buf();
    let dirs = vec![workspace.join("crates/nmp-nip29/src")];
    for dir in &dirs {
        assert!(
            dir.is_dir(),
            "expected a NIP-29 source directory at {} -- if the layout moved, \
             this gate must move with it rather than silently scanning nothing",
            dir.display()
        );
    }
    dirs
}

fn scan(dir: &Path, violations: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("a NIP-29 source dir must be readable") {
        let path = entry.expect("dir entry must be readable").path();
        if path.is_dir() {
            scan(&path, violations);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let content = std::fs::read_to_string(&path).expect("source file must be readable");
        for (lineno, line) in content.lines().enumerate() {
            let Some(code) = code_of(line) else {
                continue;
            };
            for banned in BANNED_IDENTIFIERS {
                if code.contains(banned) {
                    violations.push(format!(
                        "{}:{}: publication-visibility vocabulary {:?} in NIP-29 code: {}",
                        path.display(),
                        lineno + 1,
                        banned,
                        line.trim()
                    ));
                }
            }
        }
    }
}

/// The code half of a line: doc comments and line comments are prose and are
/// allowed to name the general mechanism, which is exactly how a reader learns
/// that NIP-29 does not implement it.
fn code_of(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") {
        return None;
    }
    Some(match line.find("//") {
        Some(at) => &line[..at],
        None => line,
    })
}
