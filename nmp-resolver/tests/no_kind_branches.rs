//! Contract test 10 (M1 plan §5/§6): the structural kill guard. Greps
//! `nmp-resolver/src/**` — excluding `testkit.rs` (fixture events legitimately
//! deal in concrete kinds) — for kind-literal comparisons or `match`-on-kind
//! patterns. Any hit means the resolver grew "the kind:3 case" / "the
//! 39002 case" and the M1 kill has fired; this test exists to make that
//! failure loud rather than a silently-passing hidden branch.

use std::path::Path;

const BANNED_SUBSTRINGS: &[&str] = &["== 3", "== 39002", "kind() =="];

#[test]
fn resolver_has_no_kind_specific_branches() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src_dir = manifest_dir.join("src");
    let mut violations = Vec::new();
    scan_dir(&src_dir, &mut violations);
    assert!(
        violations.is_empty(),
        "kind-specific branch(es) found in nmp-resolver/src (excluding \
         testkit.rs) -- the M1 kill guard fired:\n{}",
        violations.join("\n")
    );
}

fn scan_dir(dir: &Path, violations: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("src dir must be readable") {
        let entry = entry.expect("dir entry must be readable");
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, violations);
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
        }
    }
}
