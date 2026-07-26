//! Driver for the #818 Fjall journal-write-error falsifier.
//!
//! Builds each pinned release probe from its own committed lockfile, runs the
//! shared probe body in a child process, and parses the evidence records. The
//! expected matrix itself lives in `tests/journal_write_fault.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The three releases under test, with the exact registry identities recorded
/// in #818 on 2026-07-25.
///
/// These are asserted against each package's committed `Cargo.lock`, so a
/// semver-compatible bump cannot quietly substitute a different release for the
/// one this regression actually qualified.
pub const RELEASES: [Release; 3] = [
    Release {
        version: "3.1.6",
        package: "v3_1_6",
        role: Role::NegativeControl,
        fjall_checksum: "9fcdc69609906151dff9b534e30eaf8515082055d36f628e382bd0b5d6a1d362",
        lsm_tree_checksum: "39ca67401338b98d58447387dd5230552d2241bc388206e491d137b18dfea9d6",
    },
    Release {
        version: "3.1.7",
        package: "v3_1_7",
        role: Role::FixIntroduction,
        fjall_checksum: "f11ea8b671d9e2c523a90e4afc0de9fea88db692102c169151294c497ccd9d8c",
        lsm_tree_checksum: "5be54ebbcc23bff0c39c73c466d6320475f9dc7590d7e52ebba1d257fe9ded00",
    },
    Release {
        version: "3.1.8",
        package: "v3_1_8",
        role: Role::Candidate,
        fjall_checksum: "420a84699b8ccbb1ed573e38e88f4f23637b45beab6432066452f834be469c57",
        lsm_tree_checksum: "055a908d502129cf63bedae52f2db222e4436d2da32a69df9b84ac9fb9147761",
    },
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// 3.1.6 -- discards the journal `write_batch` result.
    NegativeControl,
    /// 3.1.7 -- where the `?` was introduced.
    FixIntroduction,
    /// 3.1.8 -- the release actually being qualified. Its result is observed
    /// directly and is never inferred from 3.1.7.
    Candidate,
}

#[derive(Clone, Copy)]
pub struct Release {
    pub version: &'static str,
    pub package: &'static str,
    pub role: Role,
    pub fjall_checksum: &'static str,
    pub lsm_tree_checksum: &'static str,
}

/// Parsed `KEY=value` evidence from one probe run.
pub struct Evidence {
    pub records: BTreeMap<String, String>,
    pub refusals: Vec<String>,
    pub exit_code: Option<i32>,
    pub raw: String,
}

impl Evidence {
    pub fn get(&self, key: &str) -> &str {
        self.records
            .get(key)
            .unwrap_or_else(|| panic!("probe emitted no {key} record\n{}", self.raw))
    }

    pub fn number(&self, key: &str) -> u64 {
        self.get(key)
            .parse()
            .unwrap_or_else(|_| panic!("{key} is not a number\n{}", self.raw))
    }

    pub fn flag(&self, key: &str) -> bool {
        match self.get(key) {
            "true" => true,
            "false" => false,
            other => panic!("{key} is not a boolean: {other}\n{}", self.raw),
        }
    }

    /// Exact rows, as `keyspace/keyhex=valuehex`. Comparisons in the matrix are
    /// over these, never over row counts.
    pub fn state(&self, key: &str) -> Vec<&str> {
        let raw = self.get(key);
        if raw.is_empty() {
            Vec::new()
        } else {
            raw.split(',').collect()
        }
    }

    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && self.refusals.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Expected fixture state, constructed independently of the probe
// ---------------------------------------------------------------------------
//
// These mirror the fixture constants and key/value formulas in
// `../shared/probe.rs`. The duplication is deliberate: asserting an observed
// state against a value the probe also produced would only prove the probe is
// self-consistent. Re-deriving the expected rows here means a dropped row, a
// corrupted value, or an unrelated extra row fails the comparison.
//
// If the probe's fixture changes, these must change with it, and the tests fail
// loudly until they do -- which is the intended behaviour for a falsifier whose
// fixture is part of the evidence.

/// Keyspaces the target transaction spans. Mirrors `probe.rs::KEYSPACES`.
pub const KEYSPACES: [&str; 3] = ["alpha", "beta", "gamma"];
/// Mirrors `probe.rs::PRE_STATE_ROWS`.
pub const PRE_STATE_ROWS: usize = 4;
/// Mirrors `probe.rs::TARGET_ROWS_PER_KEYSPACE`.
pub const TARGET_ROWS_PER_KEYSPACE: usize = 4;
/// Mirrors `probe.rs::TARGET_VALUE_BYTES`.
pub const TARGET_VALUE_BYTES: usize = 1_024;
/// Mirrors `probe.rs::UNDERSIZED_ROWS_PER_KEYSPACE`.
pub const UNDERSIZED_ROWS_PER_KEYSPACE: usize = 1;
/// Mirrors `probe.rs::UNDERSIZED_VALUE_BYTES`.
pub const UNDERSIZED_VALUE_BYTES: usize = 700;

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Mirrors `probe.rs::target_value`.
fn target_value(keyspace: &str, index: usize, width: usize) -> Vec<u8> {
    let seed = format!("target-value/{keyspace}/{index:04}/");
    let mut value = Vec::with_capacity(width);
    while value.len() < width {
        let remaining = width - value.len();
        let chunk = seed.as_bytes();
        if chunk.len() <= remaining {
            value.extend_from_slice(chunk);
        } else {
            value.extend_from_slice(&chunk[..remaining]);
        }
    }
    value
}

/// Serialises rows the way the probe does: grouped by keyspace, ordered by key,
/// rendered as `keyspace/keyhex=valuehex`.
fn encode(rows: BTreeMap<String, BTreeMap<String, String>>) -> Vec<String> {
    let mut out = Vec::new();
    for (keyspace, keyed) in rows {
        for (key, value) in keyed {
            out.push(format!("{keyspace}/{key}={value}"));
        }
    }
    out
}

/// The exact baseline written and synced before the fault is armed.
pub fn expected_pre_state() -> Vec<String> {
    let mut rows = BTreeMap::new();
    for keyspace in KEYSPACES {
        let mut keyed = BTreeMap::new();
        for index in 0..PRE_STATE_ROWS {
            keyed.insert(
                hex(format!("pre/{keyspace}/{index:04}").as_bytes()),
                hex(format!("pre-value/{keyspace}/{index:04}").as_bytes()),
            );
        }
        rows.insert(keyspace.to_owned(), keyed);
    }
    encode(rows)
}

/// The exact state once the target transaction has been applied: the baseline
/// plus every target row, byte for byte.
///
/// This is what a release that acknowledges the failed journal write exposes
/// in-process, and what a healthy commit leaves behind.
pub fn expected_post_state(rows_per_keyspace: usize, value_bytes: usize) -> Vec<String> {
    let mut rows = BTreeMap::new();
    for keyspace in KEYSPACES {
        let mut keyed = BTreeMap::new();
        for index in 0..PRE_STATE_ROWS {
            keyed.insert(
                hex(format!("pre/{keyspace}/{index:04}").as_bytes()),
                hex(format!("pre-value/{keyspace}/{index:04}").as_bytes()),
            );
        }
        for index in 0..rows_per_keyspace {
            keyed.insert(
                hex(format!("target/{keyspace}/{index:04}").as_bytes()),
                hex(&target_value(keyspace, index, value_bytes)),
            );
        }
        rows.insert(keyspace.to_owned(), keyed);
    }
    encode(rows)
}

fn workspace_root() -> PathBuf {
    // `harness/` sits beside the three release packages.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("harness has a parent directory")
        .to_path_buf()
}

fn manifest(release: &Release) -> PathBuf {
    workspace_root().join(release.package).join("Cargo.toml")
}

/// Asserts the committed lockfile still pins the exact release identities
/// recorded in #818. This is the anti-rot check: the regression is only
/// evidence about the versions it actually linked.
pub fn verify_pinned_identity(release: &Release) -> Result<(), String> {
    let lock_path = workspace_root().join(release.package).join("Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .map_err(|error| format!("read {}: {error}", lock_path.display()))?;

    for (crate_name, expected_checksum) in [
        ("fjall", release.fjall_checksum),
        ("lsm-tree", release.lsm_tree_checksum),
    ] {
        let block = lock
            .split("[[package]]")
            .find(|block| block.contains(&format!("name = \"{crate_name}\"")))
            .ok_or_else(|| format!("{crate_name} missing from {}", lock_path.display()))?;
        let version = format!("version = \"{}\"", release.version);
        if !block.contains(&version) {
            return Err(format!(
                "{crate_name} in {} is not pinned to {}",
                lock_path.display(),
                release.version
            ));
        }
        if !block.contains(expected_checksum) {
            return Err(format!(
                "{crate_name} {} in {} does not match the checksum recorded in #818 ({expected_checksum})",
                release.version,
                lock_path.display(),
            ));
        }
    }
    Ok(())
}

/// Builds one release probe from its own lockfile.
///
/// `--locked` is deliberate: a run that would have to update the lockfile is a
/// run whose crate identities are not the ones #818 recorded, and it fails
/// rather than silently re-resolving.
pub fn build(release: &Release) -> Result<PathBuf, String> {
    let manifest = manifest(release);
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .arg("build")
        .arg("--locked")
        .arg("--manifest-path")
        .arg(&manifest)
        // The probe packages are detached workspaces with their own target
        // directories; an inherited target dir would collide across releases.
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("RUSTFLAGS")
        .output()
        .map_err(|error| format!("spawn cargo build for {}: {error}", release.version))?;
    if !output.status.success() {
        return Err(format!(
            "cargo build failed for fjall {}\n{}",
            release.version,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let binary = workspace_root()
        .join(release.package)
        .join("target/debug")
        .join(format!("fjall-journal-fault-{}", release.package));
    if !binary.exists() {
        return Err(format!("probe binary missing at {}", binary.display()));
    }
    Ok(binary)
}

/// Runs one probe mode in its own child process and directory.
///
/// A child is mandatory, not stylistic: `RLIMIT_FSIZE` and signal disposition
/// are process-global, so the fault must not escape into the test runner.
pub fn run(release: &Release, mode: &str, directory: &Path) -> Result<Evidence, String> {
    let binary = build(release)?;
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("create {}: {error}", directory.display()))?;

    let output = Command::new(&binary)
        .arg(mode)
        .arg(directory)
        .output()
        .map_err(|error| format!("spawn probe {} {mode}: {error}", release.version))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let raw = format!(
        "--- fjall {} / {mode} (exit {:?}) ---\nstdout:\n{stdout}\nstderr:\n{stderr}",
        release.version,
        output.status.code()
    );

    let mut records = BTreeMap::new();
    let mut refusals = Vec::new();
    for line in stdout.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key == "REFUSE" {
            refusals.push(value.to_owned());
        } else {
            records.insert(key.to_owned(), value.to_owned());
        }
    }

    // A probe that died on a signal recorded nothing trustworthy.
    if output.status.code().is_none() {
        return Err(format!("probe terminated by signal\n{raw}"));
    }

    // Echo the evidence on every run, not only on failure. The child's stdout is
    // captured so the fault cannot escape into the test runner, which means
    // without this the records would be visible only inside a panic message and
    // `--nocapture` would have nothing to uncapture. State rows are elided --
    // they are kilobytes of hex, and the assertions compare them exactly.
    println!("--- fjall {} / {mode} ---", release.version);
    for (key, value) in &records {
        if key.starts_with("STATE_") {
            println!(
                "  {key}=<{} rows>",
                if value.is_empty() {
                    0
                } else {
                    value.matches(',').count() + 1
                }
            );
        } else {
            println!("  {key}={value}");
        }
    }
    for reason in &refusals {
        println!("  REFUSE={reason}");
    }

    Ok(Evidence {
        records,
        refusals,
        exit_code: output.status.code(),
        raw,
    })
}

/// Every mode runs against every release; the matrix in the test decides what
/// each combination must show.
pub const MODES: [&str; 6] = [
    "healthy",
    "one-shot",
    "persistent",
    "undersized",
    "undersized-sustained",
    "misinjected",
];
