use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt};
#[cfg(unix)]
use std::process::Command;

use tempfile::tempdir;

use crate::evidence::{with_injected_github_credentials, EvidenceLocator, EvidenceResolver};

#[test]
fn every_supported_kind_resolves_and_maps_to_its_lane() {
    let fixture = resolver_fixture();
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    for locator in [
        "rust:owner::rust_proof",
        "parity:owner::rust_proof",
        "swift:SwiftOwner::testSwiftProof",
        "kotlin:KotlinOwner::kotlinProof",
        "script:repository::scripts/proof.sh",
        "live:live-probe::bounded",
    ] {
        resolver
            .resolve(&EvidenceLocator::parse(locator).unwrap())
            .unwrap_or_else(|error| panic!("{locator}: {error}"));
    }
}

#[cfg(unix)]
#[test]
fn symlink_backed_script_evidence_is_not_repository_owned_proof() {
    let fixture = resolver_fixture();
    let external = tempdir().unwrap();
    let external_script = external.path().join("proof.sh");
    fs::write(&external_script, "#!/usr/bin/env bash\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&external_script).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&external_script, permissions).unwrap();

    let script = fixture.path().join("scripts/proof.sh");
    fs::remove_file(&script).unwrap();
    symlink(&external_script, &script).unwrap();

    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    assert!(resolver
        .resolve(&EvidenceLocator::parse("script:repository::scripts/proof.sh").unwrap())
        .unwrap_err()
        .0
        .contains("symlink"));
}

#[cfg(unix)]
#[test]
fn symlink_backed_workflow_is_not_repository_owned_proof() {
    let fixture = resolver_fixture();
    let external = tempdir().unwrap();
    let external_workflow = external.path().join("ci.yml");
    fs::write(
        &external_workflow,
        "on: [push, pull_request]\njobs:\n  test:\n    steps:\n      - run: cargo test --workspace\n",
    )
    .unwrap();

    let workflow = fixture.path().join(".github/workflows/ci.yml");
    fs::remove_file(&workflow).unwrap();
    symlink(&external_workflow, &workflow).unwrap();

    assert!(EvidenceResolver::new(fixture.path())
        .err()
        .expect("symlink-backed workflow must fail closed")
        .0
        .contains("symlink"));
}

#[cfg(unix)]
#[test]
fn recursive_source_walks_reject_directory_symlink_escape_chains() {
    for (owner_directory, locator) in [
        ("crates/owner/src", "rust:owner::rust_proof"),
        (
            "Packages/SwiftOwner/Tests",
            "swift:SwiftOwner::testSwiftProof",
        ),
        (
            "Packages/KotlinOwner/src/test",
            "kotlin:KotlinOwner::kotlinProof",
        ),
    ] {
        let fixture = resolver_fixture();
        let external = tempdir().unwrap();
        let external_source_tree = external.path().join("borrowed");
        fs::create_dir(&external_source_tree).unwrap();
        symlink(
            &external_source_tree,
            fixture.path().join(owner_directory).join("borrowed"),
        )
        .unwrap();

        let resolver = EvidenceResolver::new(fixture.path()).unwrap();
        assert!(
            resolver
                .resolve(&EvidenceLocator::parse(locator).unwrap())
                .unwrap_err()
                .0
                .contains("symlink"),
            "{locator} accepted a recursive source directory symlink"
        );
    }
}

#[test]
fn stale_ambiguous_and_unmapped_evidence_fail_closed() {
    let fixture = resolver_fixture();
    fs::write(
        fixture.path().join("crates/owner/src/lib.rs"),
        "#[cfg(test)]\nmod proofs;\n#[cfg(test)]\nmod other;\n",
    )
    .unwrap();
    fs::write(
        fixture.path().join("crates/owner/src/other.rs"),
        "#[test]\nfn rust_proof() {}\n",
    )
    .unwrap();
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    assert!(resolver
        .resolve(&EvidenceLocator::parse("rust:owner::missing").unwrap())
        .unwrap_err()
        .0
        .contains("found 0"));
    assert!(resolver
        .resolve(&EvidenceLocator::parse("rust:owner::rust_proof").unwrap())
        .unwrap_err()
        .0
        .contains("found 2"));
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
        .contains("enabled executable job"));

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

#[cfg(unix)]
#[test]
fn unreferenced_rust_file_is_not_compiled_or_executable_evidence() {
    const TARGET: &str = "unreachable_test_shape_must_not_count_as_evidence";
    let fixture = resolver_fixture();
    fs::write(
        fixture
            .path()
            .join("crates/owner/src/unreachable_evidence.rs"),
        format!("#[test]\nfn {TARGET}() {{ panic!(\"must never run\"); }}\n"),
    )
    .unwrap();
    let cargo_target = tempdir().unwrap();
    let output = Command::new(env!("CARGO"))
        .current_dir(fixture.path())
        .env("CARGO_TARGET_DIR", cargo_target.path())
        .args(["test", "-p", "owner", TARGET, "--", "--exact"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "filtered Cargo proof failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("running 0 tests"), "{stdout}");
    assert!(!stdout.contains(&format!("test {TARGET} ...")));

    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    assert!(resolver
        .resolve(&EvidenceLocator::parse(&format!("rust:owner::{TARGET}")).unwrap())
        .unwrap_err()
        .0
        .contains("found 0"));
}

#[test]
fn module_file_without_a_mod_edge_is_not_registered_evidence() {
    let fixture = resolver_fixture();
    fs::write(
        fixture
            .path()
            .join("crates/owner/src/missing_module_edge.rs"),
        "#[test]\nfn missing_module_proof() { panic!(\"must never run\"); }\n",
    )
    .unwrap();

    assert_rust_locator_error(&fixture, "missing_module_proof", "found 0");
}

#[test]
fn ignored_cfg_and_feature_disabled_tests_are_not_registered_evidence() {
    let fixture = resolver_fixture();
    fs::write(
        fixture.path().join("crates/owner/Cargo.toml"),
        "[package]\nname = \"owner\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [features]\ndisabled-proof = []\n",
    )
    .unwrap();
    fs::write(
        fixture.path().join("crates/owner/src/proofs.rs"),
        r#"
#[ignore]
#[test]
fn ignored_proof() { panic!("ignored proof must never qualify"); }

#[cfg(any())]
#[test]
fn cfg_disabled_proof() { panic!("disabled cfg proof must never qualify"); }

#[cfg(feature = "disabled-proof")]
#[test]
fn feature_disabled_proof() { panic!("disabled feature proof must never qualify"); }
"#,
    )
    .unwrap();
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    for target in [
        "ignored_proof",
        "cfg_disabled_proof",
        "feature_disabled_proof",
    ] {
        let error = resolver
            .resolve(&EvidenceLocator::parse(&format!("rust:owner::{target}")).unwrap())
            .unwrap_err();
        assert!(
            error.0.contains("found 0"),
            "{target} was not rejected as nonregistered evidence: {error}"
        );
    }
}

#[test]
fn local_macros_strings_and_alias_shadows_cannot_manufacture_registered_tests() {
    let fixture = resolver_fixture();
    fs::write(
        fixture.path().join("crates/owner/src/proofs.rs"),
        r##"
macro_rules! proptest {
    ($($tokens:tt)*) => {};
}
proptest! {
    #[test]
    fn local_macro_shadow() {}
}

mod lookalike {
    macro_rules! proptest {
        ($($tokens:tt)*) => {};
    }
    pub(crate) use proptest;
}
use crate::proofs::lookalike as aliased_proptest;
aliased_proptest::proptest! {
    #[test]
    fn aliased_macro_shadow() {}
}

#[test]
fn real_registered_proof() {
    let _description = "#[test] fn string_literal_shadow() {}";
}

macro_rules! registered_test {
    ($name:ident) => {
        #[test]
        fn $name() {}
    };
}
registered_test!(macro_generated_proof);
"##,
    )
    .unwrap();
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    for target in [
        "local_macro_shadow",
        "aliased_macro_shadow",
        "string_literal_shadow",
    ] {
        let error = resolver
            .resolve(&EvidenceLocator::parse(&format!("rust:owner::{target}")).unwrap())
            .unwrap_err();
        assert!(error.0.contains("found 0"), "{target}: {error}");
    }
    resolver
        .resolve(&EvidenceLocator::parse("rust:owner::macro_generated_proof").unwrap())
        .unwrap();
}

#[test]
fn evidence_cannot_be_borrowed_from_another_workspace_package() {
    let fixture = resolver_fixture();
    fs::create_dir_all(fixture.path().join("crates/other/src")).unwrap();
    fs::write(
        fixture.path().join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/owner\", \"crates/other\"]\n",
    )
    .unwrap();
    fs::write(
        fixture.path().join("crates/other/Cargo.toml"),
        "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        fixture.path().join("crates/other/src/lib.rs"),
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn borrowed_proof() {}\n}\n",
    )
    .unwrap();

    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    let error = resolver
        .resolve(&EvidenceLocator::parse("rust:owner::borrowed_proof").unwrap())
        .unwrap_err();
    assert!(error.0.contains("found 0"), "{error}");
    resolver
        .resolve(&EvidenceLocator::parse("rust:other::borrowed_proof").unwrap())
        .unwrap();
}

#[test]
fn custom_harness_targets_make_rust_evidence_fail_closed() {
    let fixture = resolver_fixture();
    fs::create_dir_all(fixture.path().join("crates/owner/tests")).unwrap();
    fs::write(
        fixture.path().join("crates/owner/Cargo.toml"),
        "[package]\nname = \"owner\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         [[test]]\nname = \"custom\"\npath = \"tests/custom.rs\"\nharness = false\n",
    )
    .unwrap();
    fs::write(
        fixture.path().join("crates/owner/tests/custom.rs"),
        "fn main() {}\n",
    )
    .unwrap();

    assert_rust_locator_error(&fixture, "rust_proof", "harness = false");
}

#[test]
fn build_scripts_run_only_across_the_credential_free_boundary() {
    let fixture = resolver_fixture();
    fs::write(
        fixture.path().join("crates/owner/Cargo.toml"),
        "[package]\nname = \"owner\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         build = \"build.rs\"\n",
    )
    .unwrap();
    fs::write(
        fixture.path().join("crates/owner/build.rs"),
        r#"
fn main() {
    assert!(std::env::var_os("GH_TOKEN").is_none(), "GH_TOKEN leaked");
    assert!(std::env::var_os("GITHUB_TOKEN").is_none(), "GITHUB_TOKEN leaked");
    std::fs::write("build-script-ran-without-credentials", b"ok").unwrap();
}

#[test]
fn build_script_token_shape_is_not_evidence() {}
"#,
    )
    .unwrap();

    with_injected_github_credentials(|| {
        let resolver = EvidenceResolver::new(fixture.path()).unwrap();
        resolver
            .resolve(&EvidenceLocator::parse("rust:owner::rust_proof").unwrap())
            .unwrap();
        let error = resolver
            .resolve(
                &EvidenceLocator::parse("rust:owner::build_script_token_shape_is_not_evidence")
                    .unwrap(),
            )
            .unwrap_err();
        assert!(error.0.contains("found 0"), "{error}");
    });
    assert_eq!(
        fs::read(
            fixture
                .path()
                .join("crates/owner/build-script-ran-without-credentials")
        )
        .unwrap(),
        b"ok"
    );
}

#[test]
fn build_script_failure_cannot_leave_registered_evidence() {
    let fixture = resolver_fixture();
    fs::write(
        fixture.path().join("crates/owner/Cargo.toml"),
        "[package]\nname = \"owner\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         build = \"build.rs\"\n",
    )
    .unwrap();
    fs::write(
        fixture.path().join("crates/owner/build.rs"),
        "fn main() { panic!(\"malicious build failure\"); }\n",
    )
    .unwrap();

    assert_rust_locator_error(&fixture, "rust_proof", "compile Cargo/libtest evidence");
}

#[cfg(unix)]
#[test]
fn libtest_list_failure_cannot_leave_registered_evidence() {
    let fixture = resolver_fixture();
    fs::write(
        fixture.path().join("crates/owner/src/proofs.rs"),
        r#"
#[test]
fn rust_proof() {}

#[cfg(target_os = "linux")]
#[used]
#[link_section = ".init_array"]
static FAIL_LIST_LINUX: extern "C" fn() = {
    extern "C" fn fail_list() {
        if std::env::args_os().any(|argument| argument == "--list") {
            std::process::exit(91);
        }
    }
    fail_list
};

#[cfg(target_os = "macos")]
#[used]
#[link_section = "__DATA,__mod_init_func"]
static FAIL_LIST_MACOS: extern "C" fn() = {
    extern "C" fn fail_list() {
        if std::env::args_os().any(|argument| argument == "--list") {
            std::process::exit(91);
        }
    }
    fail_list
};
"#,
    )
    .unwrap();

    assert_rust_locator_error(&fixture, "rust_proof", "cannot list libtest target");
}

#[test]
fn commented_workflow_commands_do_not_create_a_lane_mapping() {
    let fixture = resolver_fixture();
    fs::write(
        fixture.path().join(".github/workflows/ci.yml"),
        r#"
on: [push, pull_request]
jobs:
  test:
    steps:
      # - run: cargo test --workspace
"#,
    )
    .unwrap();
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    assert!(resolver
        .resolve(&EvidenceLocator::parse("rust:owner::rust_proof").unwrap())
        .unwrap_err()
        .0
        .contains("does not map"));
}

#[test]
fn non_run_workflow_scalar_does_not_create_a_lane_mapping() {
    let fixture = resolver_fixture();
    fs::write(
        fixture.path().join(".github/workflows/ci.yml"),
        r#"
on: [push, pull_request]
jobs:
  test:
    env:
      NOTE: cargo test --workspace
    steps:
      - run: true
"#,
    )
    .unwrap();
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    assert!(resolver
        .resolve(&EvidenceLocator::parse("rust:owner::rust_proof").unwrap())
        .unwrap_err()
        .0
        .contains("does not map"));
}

#[test]
fn deterministic_lane_must_be_required_failure_propagating_and_reachable() {
    let fixture = resolver_fixture();
    let workflow = fixture.path().join(".github/workflows/ci.yml");
    for (trigger, continue_on_error, command) in [
        ("workflow_dispatch", None, "cargo test --workspace"),
        (
            "[push, pull_request]",
            Some("true"),
            "cargo test --workspace",
        ),
        (
            "[push, pull_request]",
            Some("${{ matrix.experimental }}"),
            "cargo test --workspace",
        ),
        ("[push, pull_request]", None, "echo cargo test --workspace"),
        (
            "[push, pull_request]",
            None,
            "false && cargo test --workspace",
        ),
        (
            "[push, pull_request]",
            None,
            "cargo test --workspace || true",
        ),
        (
            "[push, pull_request]",
            None,
            "cargo test --workspace | tee test.log",
        ),
        (
            "[push, pull_request]",
            None,
            "set +e; cargo test --workspace; true",
        ),
        ("[push, pull_request]", None, "cargo test --workspace; true"),
        (
            "[push, pull_request]",
            None,
            "set +o errexit; cargo test --workspace; exit 0",
        ),
        ("[push, pull_request]", None, "cargo test --workspace; :"),
        (
            "[push, pull_request]",
            None,
            "cargo test --workspace; echo masked",
        ),
        ("[push, pull_request]", None, "cargo test --workspace &"),
        (
            "[push, pull_request]",
            None,
            "(cd .; cargo test --workspace; echo masked)",
        ),
    ] {
        fs::write(
            &workflow,
            lane_workflow(
                trigger,
                "ubuntu-latest",
                None,
                "",
                continue_on_error,
                command,
            ),
        )
        .unwrap();
        let resolver = EvidenceResolver::new(fixture.path()).unwrap();
        assert!(resolver
            .resolve(&EvidenceLocator::parse("rust:owner::rust_proof").unwrap())
            .unwrap_err()
            .0
            .contains("does not map"));
    }
}

#[test]
fn proof_step_requires_the_known_runner_shell_family_and_one_closed_command() {
    let fixture = resolver_fixture();
    let workflow = fixture.path().join(".github/workflows/ci.yml");
    for (runner, shell, command) in [
        ("macos-14", None, "cargo test --workspace"),
        // A non-Bash interpreter means the `run` scalar is not the shell
        // command this grammar reads, so it carries no lane claim.
        ("ubuntu-latest", Some("pwsh {0}"), "cargo test --workspace"),
        (
            "ubuntu-latest",
            Some("python {0}"),
            "cargo test --workspace",
        ),
        ("ubuntu-latest", None, "echo setup; cargo test --workspace"),
        ("ubuntu-latest", None, "(cd .; cargo test --workspace)"),
    ] {
        fs::write(
            &workflow,
            lane_workflow(
                "[push, pull_request]",
                runner,
                None,
                shell.unwrap_or(""),
                None,
                command,
            ),
        )
        .unwrap();
        let resolver = EvidenceResolver::new(fixture.path()).unwrap();
        assert!(resolver
            .resolve(&EvidenceLocator::parse("rust:owner::rust_proof").unwrap())
            .unwrap_err()
            .0
            .contains("does not map"));
    }

    fs::write(
        &workflow,
        lane_workflow(
            "[push, pull_request]",
            "ubuntu-latest",
            None,
            "",
            None,
            "cargo test --workspace",
        ),
    )
    .unwrap();
    EvidenceResolver::new(fixture.path())
        .unwrap()
        .resolve(&EvidenceLocator::parse("rust:owner::rust_proof").unwrap())
        .unwrap();
}

#[test]
fn command_spelling_cannot_substitute_for_provenance() {
    let fixture = resolver_fixture();
    let workflow = fixture.path().join(".github/workflows/ci.yml");
    for (locator, runner, working_directory, command) in [
        (
            "rust:owner::rust_proof",
            "ubuntu-latest",
            None,
            "cargo() { return 0; }\ncargo test --workspace",
        ),
        (
            "rust:owner::rust_proof",
            "ubuntu-latest",
            None,
            "alias cargo=true\ncargo test --workspace",
        ),
        (
            "rust:owner::rust_proof",
            "ubuntu-latest",
            None,
            "./cargo test --workspace",
        ),
        (
            "rust:owner::rust_proof",
            "ubuntu-latest",
            None,
            "/tmp/cargo test --workspace",
        ),
        (
            "rust:owner::rust_proof",
            "ubuntu-latest",
            None,
            "PATH=./shadow cargo test --workspace",
        ),
        (
            "rust:owner::rust_proof",
            "ubuntu-latest",
            None,
            "env 'BASH_FUNC_cargo%%=() { return 0; }' /bin/bash -c 'cargo test --workspace'",
        ),
        (
            "rust:owner::rust_proof",
            "ubuntu-latest",
            None,
            "function cargo { return 0; }\ncargo test --workspace",
        ),
        (
            "rust:owner::rust_proof",
            "ubuntu-lookalike",
            None,
            "cargo test --workspace",
        ),
        (
            "rust:owner::rust_proof",
            "ubuntu-latest",
            None,
            "cargo test --workspace shadow_filter",
        ),
        (
            "rust:owner::rust_proof",
            "ubuntu-latest",
            None,
            "cargo test --workspace --no-run",
        ),
        (
            "swift:SwiftOwner::testSwiftProof",
            "macos-14",
            Some("Packages/SwiftOwner"),
            "swift() { return 0; }\nswift test",
        ),
        (
            "swift:SwiftOwner::testSwiftProof",
            "macos-14",
            Some("Packages/SwiftOwner"),
            "function /usr/bin/xcrun { return 0; }\nswift test",
        ),
        (
            "swift:SwiftOwner::testSwiftProof",
            "macos-14",
            Some("Packages/SwiftOwner"),
            "PATH=./shadow swift test",
        ),
        (
            "swift:SwiftOwner::testSwiftProof",
            "macos-14",
            Some("Packages/SwiftOwner"),
            "swift test --filter shadow",
        ),
        (
            "kotlin:KotlinOwner::kotlinProof",
            "ubuntu-latest",
            Some("Packages/KotlinOwner"),
            "function ./gradlew { return 0; }\n./gradlew test",
        ),
        (
            "kotlin:KotlinOwner::kotlinProof",
            "ubuntu-latest",
            Some("Packages/KotlinOwner"),
            "gradlew test",
        ),
        (
            "kotlin:KotlinOwner::kotlinProof",
            "ubuntu-latest",
            Some("Packages/KotlinOwner"),
            "/tmp/gradlew test",
        ),
        (
            "kotlin:KotlinOwner::kotlinProof",
            "ubuntu-latest",
            Some("Packages/KotlinOwner"),
            "./gradlew test --continue",
        ),
        (
            "script:repository::scripts/proof.sh",
            "ubuntu-latest",
            None,
            "function scripts/proof.sh { return 0; }\nscripts/proof.sh",
        ),
        (
            "script:repository::scripts/proof.sh",
            "ubuntu-latest",
            None,
            "bash scripts/proof.sh",
        ),
        (
            "script:repository::scripts/proof.sh",
            "ubuntu-latest",
            None,
            "/tmp/proof.sh",
        ),
        (
            "script:repository::scripts/proof.sh",
            "ubuntu-latest",
            None,
            "scripts/proof.sh ignored-argument",
        ),
    ] {
        fs::write(
            &workflow,
            lane_workflow(
                "[push, pull_request]",
                runner,
                working_directory,
                "",
                None,
                command,
            ),
        )
        .unwrap();
        let resolver = EvidenceResolver::new(fixture.path()).unwrap();
        assert!(
            resolver
                .resolve(&EvidenceLocator::parse(locator).unwrap())
                .unwrap_err()
                .0
                .contains("does not map"),
            "{locator} accepted shadow command:\n{command}"
        );
    }
}

#[test]
fn bare_swift_function_is_not_an_executable_test() {
    let fixture = resolver_fixture();
    fs::write(
        fixture
            .path()
            .join("Packages/SwiftOwner/Tests/OwnerTests.swift"),
        "func swiftProof() {}\n",
    )
    .unwrap();
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    assert!(resolver
        .resolve(&EvidenceLocator::parse("swift:SwiftOwner::swiftProof").unwrap())
        .unwrap_err()
        .0
        .contains("executable test"));
}

#[test]
fn disabled_swift_tests_are_not_executable_evidence() {
    let fixture = resolver_fixture();
    let path = fixture
        .path()
        .join("Packages/SwiftOwner/Tests/OwnerTests.swift");
    fs::write(
        &path,
        "@Test(.disabled(\"not run\"))\nfunc testSwiftProof() {}\n",
    )
    .unwrap();
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    assert!(resolver
        .resolve(&EvidenceLocator::parse("swift:SwiftOwner::testSwiftProof").unwrap())
        .unwrap_err()
        .0
        .contains("executable test"));

    fs::write(
        &path,
        "import XCTest\nfinal class OwnerTests: XCTestCase {\n    func swiftProof() {}\n}\n",
    )
    .unwrap();
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    assert!(resolver
        .resolve(&EvidenceLocator::parse("swift:SwiftOwner::swiftProof").unwrap())
        .unwrap_err()
        .0
        .contains("executable test"));

    fs::write(
        &path,
        "import XCTest\n@available(*, unavailable)\nfinal class OwnerTests: XCTestCase {\n    func testSwiftProof() {}\n}\n",
    )
    .unwrap();
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    assert!(resolver
        .resolve(&EvidenceLocator::parse("swift:SwiftOwner::testSwiftProof").unwrap())
        .unwrap_err()
        .0
        .contains("executable test"));
}

#[test]
fn ignored_or_disabled_kotlin_test_is_not_executable_evidence() {
    let fixture = resolver_fixture();
    let path = fixture
        .path()
        .join("Packages/KotlinOwner/src/test/OwnerTest.kt");
    for disabled in ["@Ignore", "@Disabled"] {
        fs::write(
            &path,
            format!("{disabled}\n@Test\nfun kotlinProof() {{}}\n"),
        )
        .unwrap();
        let resolver = EvidenceResolver::new(fixture.path()).unwrap();
        assert!(resolver
            .resolve(&EvidenceLocator::parse("kotlin:KotlinOwner::kotlinProof").unwrap())
            .unwrap_err()
            .0
            .contains("executable test"));
    }
    fs::write(
        &path,
        "@Disabled\nclass OwnerTest {\n    @Test\n    fun kotlinProof() {}\n}\n",
    )
    .unwrap();
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    assert!(resolver
        .resolve(&EvidenceLocator::parse("kotlin:KotlinOwner::kotlinProof").unwrap())
        .unwrap_err()
        .0
        .contains("executable test"));
}

#[test]
fn native_test_comments_strings_and_lookalike_annotations_do_not_resolve() {
    let fixture = resolver_fixture();
    let swift = fixture
        .path()
        .join("Packages/SwiftOwner/Tests/OwnerTests.swift");
    let kotlin = fixture
        .path()
        .join("Packages/KotlinOwner/src/test/OwnerTest.kt");
    for source in [
        "// @Test func swiftProof() {}\n",
        "let fake = \"@Test func swiftProof() {}\"\n",
        "@NotATest\nfunc swiftProof() {}\n",
    ] {
        fs::write(&swift, source).unwrap();
        let resolver = EvidenceResolver::new(fixture.path()).unwrap();
        assert!(resolver
            .resolve(&EvidenceLocator::parse("swift:SwiftOwner::swiftProof").unwrap())
            .is_err());
    }
    for source in [
        "// @Test fun kotlinProof() {}\n",
        "val fake = \"@Test fun kotlinProof() {}\"\n",
        "@NotATest\nfun kotlinProof() {}\n",
    ] {
        fs::write(&kotlin, source).unwrap();
        let resolver = EvidenceResolver::new(fixture.path()).unwrap();
        assert!(resolver
            .resolve(&EvidenceLocator::parse("kotlin:KotlinOwner::kotlinProof").unwrap())
            .is_err());
    }
}

#[test]
fn live_workflow_requires_real_trigger_job_timeout_and_executable_step_topology() {
    let fixture = resolver_fixture();
    let path = fixture.path().join(".github/workflows/live-probe.yml");
    for source in [
        r#"
on: push
env:
  workflow_dispatch: present
not_jobs:
  bounded:
    timeout-minutes: 5
    steps:
      - run: ./probe
"#,
        r#"
on:
  workflow_dispatch:
jobs:
  other:
    timeout-minutes: 5
    steps:
      - run: ./probe
not_jobs:
  bounded:
    timeout-minutes: 5
    steps:
      - run: ./probe
"#,
        r#"
on:
  workflow_dispatch:
jobs:
  bounded:
    timeout-minutes: 5
    if: false
    steps:
      - run: ./probe
"#,
        r#"
on:
  workflow_dispatch:
jobs:
  bounded:
    timeout-minutes: 5
    steps:
      - if: false
        run: ./probe
"#,
        r#"
on:
  workflow_dispatch:
jobs:
  bounded:
    timeout-minutes: 5
    steps:
      - env:
          NOTE: ./probe
"#,
    ] {
        fs::write(&path, source).unwrap();
        let resolver = EvidenceResolver::new(fixture.path()).unwrap();
        assert!(resolver
            .resolve(&EvidenceLocator::parse("live:live-probe::bounded").unwrap())
            .unwrap_err()
            .0
            .contains("enabled executable job"));
    }
}

fn assert_rust_locator_error(fixture: &tempfile::TempDir, target: &str, expected: &str) {
    let resolver = EvidenceResolver::new(fixture.path()).unwrap();
    let error = resolver
        .resolve(&EvidenceLocator::parse(&format!("rust:owner::{target}")).unwrap())
        .unwrap_err();
    assert!(
        error.0.contains(expected),
        "expected `{expected}` while resolving `{target}`, got: {error}"
    );
}

fn lane_workflow(
    trigger: &str,
    runner: &str,
    working_directory: Option<&str>,
    shell: &str,
    continue_on_error: Option<&str>,
    command: &str,
) -> String {
    let working_directory = working_directory
        .map(|directory| format!("      working-directory: {directory}\n"))
        .unwrap_or_default();
    let shell = if shell.is_empty() {
        String::new()
    } else {
        format!("      shell: {shell}\n")
    };
    let continue_on_error = continue_on_error
        .map(|value| format!("      continue-on-error: {value}\n"))
        .unwrap_or_default();
    let command = command
        .lines()
        .map(|line| format!("        {line}\n"))
        .collect::<String>();
    format!(
        "on: {trigger}\njobs:\n  proof:\n    runs-on: {runner}\n    steps:\n    -\n{working_directory}{shell}{continue_on_error}      run: |\n{command}"
    )
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
        "#[cfg(test)]\nmod proofs;\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("crates/owner/src/proofs.rs"),
        "#[test]\nfn rust_proof() {}\n",
    )
    .unwrap();
    fs::write(
        temp.path()
            .join("Packages/SwiftOwner/Tests/OwnerTests.swift"),
        "import XCTest\nfinal class OwnerTests: XCTestCase {\n    func testSwiftProof() {}\n}\n",
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
on: [push, pull_request]
jobs:
  rust:
    runs-on: ubuntu-latest
    steps:
      - run: cargo test --workspace
  swift:
    runs-on: macos-14
    steps:
      - working-directory: Packages/SwiftOwner
        run: swift test
  kotlin:
    runs-on: ubuntu-latest
    steps:
      - working-directory: Packages/KotlinOwner
        run: ./gradlew test
  script:
    runs-on: ubuntu-latest
    steps:
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
      - run: "./probe"
"#,
    )
    .unwrap();
    temp
}
