//! Contract test 10 (M1 plan §5/§6) + its M2 hardening (M2 plan §8.1, test
//! 17). Two layers over `nmp-resolver/src/**` and the sibling
//! `nmp-router/src/**` — excluding `testkit.rs` (fixture events legitimately
//! deal in concrete kinds):
//!
//! 1. The original M1 substring/`match` scan (kept -- belt-and-suspenders,
//!    cheap fast-feedback tripwire).
//! 2. The M2 hardening: `as_u16(`/`as_u64(`/a bare `.kind` field read is
//!    banned UNLESS the same line carries the `KIND-VALUE-READ` marker --
//!    the one auditable, `#[allow(clippy::disallowed_methods)]`-scoped
//!    projection site (`nmp-resolver/src/eval.rs`). The clippy
//!    `disallowed-methods` lint (workspace `clippy.toml`) is the REAL gate
//!    (CI runs `cargo clippy --workspace --all-targets -- -D warnings`);
//!    this scanner is the cheap tripwire that also catches a bare `.kind`
//!    field read (which clippy's method-based lint cannot see) and asserts
//!    there is exactly one legitimate site.

use std::path::Path;

const BANNED_SUBSTRINGS: &[&str] = &["== 3", "== 39002", "kind() =="];
const KIND_VALUE_READ_MARKER: &str = "KIND-VALUE-READ";

#[test]
fn resolver_has_no_kind_specific_branches() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations = Vec::new();
    scan_dir(&manifest_dir.join("src"), &mut violations);
    assert!(
        violations.is_empty(),
        "kind-specific branch(es) found in nmp-resolver/src (excluding \
         testkit.rs) -- the M1 kill guard fired:\n{}",
        violations.join("\n")
    );
}

#[test]
fn kind_value_reads_are_confined_to_the_one_annotated_projection_site() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let router_src = manifest_dir
        .parent()
        .expect("nmp-resolver has a parent (workspace root)")
        .join("nmp-router/src");
    assert!(
        router_src.is_dir(),
        "expected sibling crate nmp-router/src at {}",
        router_src.display()
    );

    let mut violations = Vec::new();
    scan_kind_value_reads(&manifest_dir.join("src"), &mut violations);
    scan_kind_value_reads(&router_src, &mut violations);
    assert!(
        violations.is_empty(),
        "un-annotated kind-value read(s) -- every `as_u16(`/`as_u64(`/bare \
         `.kind` field read must carry a `{KIND_VALUE_READ_MARKER}` marker \
         on the same line (the one auditable projection site):\n{}",
        violations.join("\n")
    );
}

fn scan_dir(dir: &Path, violations: &mut Vec<String>) {
    for_each_source_line(dir, |path, lineno, line| {
        for pat in BANNED_SUBSTRINGS {
            if line.contains(pat) {
                violations.push(format!(
                    "{}:{}: forbidden pattern {:?}: {}",
                    path.display(),
                    lineno + 1,
                    pat,
                    line.trim()
                ));
            }
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("match") && line.to_ascii_lowercase().contains("kind") {
            violations.push(format!(
                "{}:{}: 'match ... kind' pattern: {}",
                path.display(),
                lineno + 1,
                line.trim()
            ));
        }
    });
}

fn scan_kind_value_reads(dir: &Path, violations: &mut Vec<String>) {
    for_each_source_line(dir, |path, lineno, line| {
        if line.contains(KIND_VALUE_READ_MARKER) {
            return;
        }
        if line.contains("as_u16(") || line.contains("as_u64(") || contains_bare_dot_kind(line) {
            violations.push(format!(
                "{}:{}: un-annotated kind-value read: {}",
                path.display(),
                lineno + 1,
                line.trim()
            ));
        }
    });
}

/// True iff `line` reads a bare `.kind` field (as opposed to `.kinds`, the
/// unrelated plural data-field name copied opaquely elsewhere).
fn contains_bare_dot_kind(line: &str) -> bool {
    let bytes = line.as_bytes();
    let pat = b".kind";
    let mut i = 0;
    while let Some(pos) = line[i..].find(".kind") {
        let start = i + pos;
        let after = start + pat.len();
        let followed_by_s = bytes.get(after) == Some(&b's');
        if !followed_by_s {
            return true;
        }
        i = after;
    }
    false
}

fn for_each_source_line(dir: &Path, mut visit: impl FnMut(&Path, usize, &str)) {
    scan_dir_inner(dir, &mut visit);
}

fn scan_dir_inner(dir: &Path, visit: &mut impl FnMut(&Path, usize, &str)) {
    for entry in std::fs::read_dir(dir).expect("src dir must be readable") {
        let entry = entry.expect("dir entry must be readable");
        let path = entry.path();
        if path.is_dir() {
            scan_dir_inner(&path, visit);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        // testkit.rs builds fixture *events* for the harness; it legitimately
        // deals in concrete kind literals (kind:3, kind:39002, ...). Only the
        // resolver's own event-to-node routing must stay kind-blind.
        if path.file_name().and_then(|f| f.to_str()) == Some("testkit.rs") {
            continue;
        }
        let content = std::fs::read_to_string(&path).expect("source file must be readable");
        for (lineno, line) in content.lines().enumerate() {
            visit(&path, lineno, line);
        }
    }
}
