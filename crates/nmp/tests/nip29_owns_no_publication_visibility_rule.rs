//! #1182 falsifier 5: no file under `crates/nmp-nip29/` or
//! `crates/nmp/src/nip29/` special-cases publication visibility.
//!
//! Whether a locally accepted write appears in a matching live query, and what
//! provenance it reports while no relay has carried it, is ordinary engine
//! behaviour that every protocol gets for free. NIP-29 is a vocabulary of
//! kinds and a per-relay authority rule; it has no opinion about the outbound
//! publication queue and must never acquire one.
//!
//! The gate is a source scan for the vocabulary such a special case would have
//! to be written in: reading a row's provenance, reading local-write origin, or
//! reaching into the store's projection doors. NIP-29 code mints `Demand`s and
//! validates `h` tags; it never inspects where a row came from. If a branch
//! ever appears there, the term it needs shows up here first.
//!
//! Prose is exempt: `discovery.rs` legitimately EXPLAINS the per-relay
//! authority rule and points at the general engine mechanism by name. Only
//! code is scanned -- a documentation reference is the opposite of a special
//! case.

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
    // The delivered row's source set: branching on it is a visibility rule.
    ".sources",
];

#[test]
fn nip29_code_never_names_publication_visibility_vocabulary() {
    let mut violations = Vec::new();
    for dir in nip29_dirs() {
        scan(&dir, &mut violations);
    }
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

fn nip29_dirs() -> Vec<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/nmp has a parent")
        .parent()
        .expect("crates has a parent (workspace root)")
        .to_path_buf();
    let dirs = vec![
        workspace.join("crates/nmp-nip29/src"),
        workspace.join("crates/nmp/src/nip29"),
    ];
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
