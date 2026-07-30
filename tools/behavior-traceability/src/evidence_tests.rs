use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tempfile::tempdir;

use crate::evidence::{EvidenceLocator, EvidenceResolver};

#[test]
fn every_supported_kind_resolves_and_maps_to_its_lane() {
    let fixture = resolver_fixture();
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    for locator in [
        "rust:owner::rust_proof",
        "parity:owner::rust_proof",
        "swift:SwiftOwner::swiftProof",
        "kotlin:KotlinOwner::kotlinProof",
        "script:repository::scripts/proof.sh",
        "live:live-probe::bounded",
    ] {
        resolver
            .resolve(&EvidenceLocator::parse(locator).unwrap())
            .unwrap_or_else(|error| panic!("{locator}: {error}"));
    }
}

#[test]
fn stale_ambiguous_and_unmapped_evidence_fail_closed() {
    let fixture = resolver_fixture();
    fs::write(
        fixture.path().join("crates/owner/src/other.rs"),
        "fn rust_proof() {}\n",
    )
    .unwrap();
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    assert!(resolver
        .resolve(&EvidenceLocator::parse("rust:owner::missing").unwrap())
        .unwrap_err()
        .0
        .contains("0 same-named"));
    assert!(resolver
        .resolve(&EvidenceLocator::parse("rust:owner::rust_proof").unwrap())
        .unwrap_err()
        .0
        .contains("2 same-named"));
    assert!(resolver
        .resolve(&EvidenceLocator::parse("script:repository::proof.sh").unwrap())
        .unwrap_err()
        .0
        .contains("slash-qualified"));
    assert!(resolver
        .resolve(&EvidenceLocator::parse("swift:swiftowner::swiftProof").unwrap())
        .unwrap_err()
        .0
        .contains("no exact"));
    assert!(resolver
        .resolve(&EvidenceLocator::parse("parity:missing-owner::rust_proof").unwrap())
        .unwrap_err()
        .0
        .contains("not an exact workspace package"));
    assert!(resolver
        .resolve(&EvidenceLocator::parse("live:live-probe::missing_job").unwrap())
        .unwrap_err()
        .0
        .contains("must name a job"));

    fs::write(
        fixture.path().join(".github/workflows/ci.yml"),
        "jobs: {}\n",
    )
    .unwrap();
    let unmapped = EvidenceResolver::new(fixture.path()).unwrap();
    assert!(unmapped
        .resolve(&EvidenceLocator::parse("script:repository::scripts/proof.sh").unwrap())
        .unwrap_err()
        .0
        .contains("does not map"));
}

fn resolver_fixture() -> tempfile::TempDir {
    let temp = tempdir().unwrap();
    for path in [
        "crates/owner/src",
        "Packages/SwiftOwner/Tests",
        "Packages/KotlinOwner/src/test",
        "scripts",
        ".github/workflows",
    ] {
        fs::create_dir_all(temp.path().join(path)).unwrap();
    }
    fs::write(
        temp.path().join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/owner\"]\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("crates/owner/Cargo.toml"),
        "[package]\nname = \"owner\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("crates/owner/src/lib.rs"),
        "#[test]\nfn rust_proof() {}\n",
    )
    .unwrap();
    fs::write(
        temp.path()
            .join("Packages/SwiftOwner/Tests/OwnerTests.swift"),
        "func swiftProof() {}\n",
    )
    .unwrap();
    fs::write(
        temp.path()
            .join("Packages/KotlinOwner/src/test/OwnerTest.kt"),
        "@Test\nfun kotlinProof() {}\n",
    )
    .unwrap();
    let script = temp.path().join("scripts/proof.sh");
    fs::write(&script, "#!/usr/bin/env bash\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
    }
    fs::write(
        temp.path().join(".github/workflows/ci.yml"),
        r#"
jobs:
  test:
    steps:
      - run: cargo test --workspace
      - working-directory: Packages/SwiftOwner
        run: swift test
      - working-directory: Packages/KotlinOwner
        run: ./gradlew test
      - run: scripts/proof.sh
"#,
    )
    .unwrap();
    fs::write(
        temp.path().join(".github/workflows/live-probe.yml"),
        r#"
on:
  workflow_dispatch:
jobs:
  bounded:
    timeout-minutes: 5
    steps:
      - run: true
"#,
    )
    .unwrap();
    temp
}
