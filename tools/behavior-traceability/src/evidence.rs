use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use cargo_metadata::{Message, Metadata, PackageId, Target, TargetKind};
use serde_yaml_ng::{Mapping as YamlMapping, Value as YamlValue};
use tempfile::TempDir;

use crate::model::TraceError;

#[cfg(test)]
thread_local! {
    static INJECT_GITHUB_CREDENTIALS: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceLocator {
    pub kind: EvidenceKind,
    pub owner: String,
    pub target: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceKind {
    Rust,
    Swift,
    Kotlin,
    Parity,
    Script,
    Live,
}

impl EvidenceLocator {
    pub fn parse(value: &str) -> Result<Self, TraceError> {
        let (kind, owner_target) = value.split_once(':').ok_or_else(|| {
            TraceError(format!(
                "invalid evidence `{value}`; expected <kind>:<owner>::<target>"
            ))
        })?;
        let (owner, target) = owner_target.split_once("::").ok_or_else(|| {
            TraceError(format!(
                "invalid evidence `{value}`; expected <kind>:<owner>::<target>"
            ))
        })?;
        if owner.is_empty()
            || target.is_empty()
            || owner.chars().any(char::is_whitespace)
            || target.chars().any(char::is_whitespace)
        {
            return Err(TraceError(format!(
                "invalid evidence `{value}`; owner and target must be non-empty and whitespace-free"
            )));
        }
        let kind = match kind {
            "rust" => EvidenceKind::Rust,
            "swift" => EvidenceKind::Swift,
            "kotlin" => EvidenceKind::Kotlin,
            "parity" => EvidenceKind::Parity,
            "script" => EvidenceKind::Script,
            "live" => EvidenceKind::Live,
            other => {
                return Err(TraceError(format!(
                    "unsupported evidence kind `{other}` in `{value}`"
                )))
            }
        };
        Ok(Self {
            kind,
            owner: owner.to_owned(),
            target: target.to_owned(),
        })
    }

    pub fn is_facade_proof(&self) -> bool {
        self.kind == EvidenceKind::Rust && self.owner == "nmp"
    }
}

pub(crate) struct EvidenceResolver {
    root: PathBuf,
    packages: BTreeMap<String, RustPackage>,
    workflows: BTreeMap<String, Workflow>,
    cargo_state: CargoState,
}

struct RustPackage {
    root: PathBuf,
    id: PackageId,
    manifest: PathBuf,
    registered_tests: OnceCell<Result<BTreeMap<String, usize>, TraceError>>,
}

struct CargoState {
    _directory: TempDir,
    cargo: PathBuf,
    cargo_home: PathBuf,
    target: PathBuf,
}

struct Workflow {
    document: YamlValue,
}

struct WorkflowRunStep<'a> {
    command: &'a str,
    working_directory: Option<&'a str>,
    runner: &'a str,
    shell: Option<&'a str>,
}

impl EvidenceResolver {
    pub(crate) fn new(root: &Path) -> Result<Self, TraceError> {
        let root = fs::canonicalize(root).map_err(|error| {
            TraceError(format!(
                "cannot canonicalize repository root {}: {error}",
                root.display()
            ))
        })?;
        repository_regular_file(&root, &root.join("Cargo.toml"), "workspace Cargo manifest")?;
        if root.join("Cargo.lock").exists() {
            repository_regular_file(&root, &root.join("Cargo.lock"), "workspace Cargo lockfile")?;
        }
        let cargo_state = CargoState::new(&root)?;
        let metadata = cargo_metadata(&root, &cargo_state)?;
        let workspace_members = metadata
            .workspace_members
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut packages = BTreeMap::new();
        for package in metadata
            .packages
            .into_iter()
            .filter(|package| workspace_members.contains(&package.id))
        {
            let manifest = package.manifest_path.into_std_path_buf();
            let rust_package = RustPackage {
                root: manifest
                    .parent()
                    .expect("Cargo manifest has a parent")
                    .to_path_buf(),
                id: package.id,
                manifest,
                registered_tests: OnceCell::new(),
            };
            if packages
                .insert(package.name.to_string(), rust_package)
                .is_some()
            {
                return Err(TraceError(format!(
                    "workspace has duplicate package name `{}`",
                    package.name
                )));
            }
        }
        let workflows = load_workflows(&root)?;
        Ok(Self {
            root,
            packages,
            workflows,
            cargo_state,
        })
    }

    pub(crate) fn resolve(&self, locator: &EvidenceLocator) -> Result<(), TraceError> {
        match locator.kind {
            EvidenceKind::Rust | EvidenceKind::Parity => self.resolve_rust(locator)?,
            EvidenceKind::Swift => self.resolve_swift(locator)?,
            EvidenceKind::Kotlin => self.resolve_kotlin(locator)?,
            EvidenceKind::Script => self.resolve_script(locator)?,
            EvidenceKind::Live => self.resolve_live(locator)?,
        }
        self.require_lane(locator)
    }

    fn resolve_rust(&self, locator: &EvidenceLocator) -> Result<(), TraceError> {
        let package = self.packages.get(&locator.owner).ok_or_else(|| {
            TraceError(format!(
                "Rust evidence owner `{}` is not an exact workspace package",
                locator.owner
            ))
        })?;
        if !is_identifier(&locator.target) {
            return Err(TraceError(format!(
                "Rust evidence target `{}` must be one exact function identifier",
                locator.target
            )));
        }
        // Preserve the repository-ownership/symlink audit across the package,
        // but executable reachability comes only from Cargo/libtest below.
        let mut audited_paths = Vec::new();
        collect_files(&self.root, &package.root, "rs", &mut audited_paths)?;
        let tests = package
            .registered_tests
            .get_or_init(|| self.compile_registered_tests(package))
            .as_ref()
            .map_err(Clone::clone)?;
        let matches = tests
            .iter()
            .filter(|(name, _)| name.rsplit("::").next() == Some(locator.target.as_str()))
            .map(|(_, count)| *count)
            .sum::<usize>();
        if matches != 1 {
            return Err(TraceError(format!(
                "Rust evidence {}:{}::{} must resolve to exactly one Cargo/libtest-registered executable proof; found {}",
                match locator.kind {
                    EvidenceKind::Parity => "parity",
                    _ => "rust",
                },
                locator.owner,
                locator.target,
                matches
            )));
        }
        Ok(())
    }

    fn resolve_swift(&self, locator: &EvidenceLocator) -> Result<(), TraceError> {
        let packages = self.root.join("Packages");
        let Some(package) = exact_child_directory(&packages, &locator.owner) else {
            return Err(TraceError(format!(
                "Swift evidence owner `{}` has no exact Packages/{}/Tests root",
                locator.owner, locator.owner
            )));
        };
        let tests = package.join("Tests");
        if !tests.is_dir() {
            return Err(TraceError(format!(
                "Swift evidence owner `{}` has no exact Packages/{}/Tests root",
                locator.owner, locator.owner
            )));
        }
        unique_native_test(
            &self.root,
            &tests,
            "swift",
            "func",
            &locator.target,
            "Swift",
        )
    }

    fn resolve_kotlin(&self, locator: &EvidenceLocator) -> Result<(), TraceError> {
        let packages = self.root.join("Packages");
        let package = exact_child_directory(&packages, &locator.owner);
        if package.is_none() {
            return Err(TraceError(format!(
                "Kotlin evidence owner `{}` has no exact Packages/{} root",
                locator.owner, locator.owner
            )));
        }
        unique_native_test(
            &self.root,
            &package.expect("checked exact Kotlin package"),
            "kt",
            "fun",
            &locator.target,
            "Kotlin",
        )
    }

    fn resolve_script(&self, locator: &EvidenceLocator) -> Result<(), TraceError> {
        if locator.owner != "repository" {
            return Err(TraceError(
                "script evidence owner must be exactly `repository`".into(),
            ));
        }
        let relative = Path::new(&locator.target);
        if relative.is_absolute()
            || !locator.target.contains('/')
            || relative
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(TraceError(format!(
                "script target `{}` must be a slash-qualified repository-relative path",
                locator.target
            )));
        }
        let path = self.root.join(relative);
        let metadata = repository_regular_file(&self.root, &path, "script evidence target")?;
        if !metadata.is_file() || !is_executable(&metadata) {
            return Err(TraceError(format!(
                "script evidence target {} is not an executable file",
                path.display()
            )));
        }
        Ok(())
    }

    fn resolve_live(&self, locator: &EvidenceLocator) -> Result<(), TraceError> {
        let workflow = self.workflows.get(&locator.owner).ok_or_else(|| {
            TraceError(format!(
                "live evidence owner `{}` is not an exact workflow filename stem",
                locator.owner
            ))
        })?;
        if !workflow.has_dispatch() || !workflow.live_job_is_bounded_and_executable(&locator.target)
        {
            return Err(TraceError(format!(
                "live evidence `{}`::`{}` must name an enabled executable job under `jobs` in an `on.workflow_dispatch` workflow with positive timeout-minutes",
                locator.owner, locator.target
            )));
        }
        Ok(())
    }

    fn compile_registered_tests(
        &self,
        package: &RustPackage,
    ) -> Result<BTreeMap<String, usize>, TraceError> {
        repository_regular_file(&self.root, &package.manifest, "Cargo manifest")?;
        let manifest = read_manifest(&package.manifest)?;
        let mut command = cargo_command(&self.cargo_state);
        command.args([
            "test",
            "--no-run",
            "--message-format=json-render-diagnostics",
            "--manifest-path",
        ]);
        command.arg(self.root.join("Cargo.toml"));
        if self.root.join("Cargo.lock").is_file() {
            command.arg("--locked");
        }
        command.args(["--package", &package.id.repr]);
        let output = run_bounded(
            command,
            &format!("compile Cargo/libtest evidence for {}", package.id),
            Duration::from_secs(600),
            32 * 1024 * 1024,
        )?;
        if !output.status.success() {
            return Err(command_failure(
                &format!("compile Cargo/libtest evidence for {}", package.id),
                &output,
            ));
        }

        let mut harnesses = BTreeMap::new();
        let mut build_finished = false;
        for message in Message::parse_stream(Cursor::new(&output.stdout)) {
            let message = message.map_err(|error| {
                TraceError(format!(
                    "cannot parse Cargo compiler message for {}: {error}",
                    package.id
                ))
            })?;
            match message {
                Message::CompilerArtifact(artifact)
                    if artifact.package_id == package.id && artifact.profile.test =>
                {
                    if target_uses_custom_harness(&manifest, &artifact.target)? {
                        return Err(TraceError(format!(
                            "Cargo test target `{}` for {} declares `harness = false`",
                            artifact.target.name, package.id
                        )));
                    }
                    let executable = artifact.executable.ok_or_else(|| {
                        TraceError(format!(
                            "test-profile artifact `{}` for {} has no executable libtest harness",
                            artifact.target.name, package.id
                        ))
                    })?;
                    let executable = executable.into_std_path_buf();
                    let metadata = fs::symlink_metadata(&executable).map_err(|error| {
                        TraceError(format!(
                            "cannot inspect emitted libtest harness {}: {error}",
                            executable.display()
                        ))
                    })?;
                    if metadata.file_type().is_symlink() || !metadata.is_file() {
                        return Err(TraceError(format!(
                            "emitted libtest harness {} is not one regular file",
                            executable.display()
                        )));
                    }
                    harnesses.insert(executable, artifact.target.name);
                }
                Message::BuildFinished(finished) => build_finished = finished.success,
                _ => {}
            }
        }
        if !build_finished {
            return Err(TraceError(format!(
                "Cargo did not report a successful completed test build for {}",
                package.id
            )));
        }
        if harnesses.is_empty() {
            return Err(TraceError(format!(
                "Cargo emitted no normal libtest harness for {}",
                package.id
            )));
        }

        let mut registered = BTreeMap::new();
        for (harness, target_name) in harnesses {
            let all =
                list_libtest_harness(&self.cargo_state, &harness, false).map_err(|error| {
                    TraceError(format!(
                        "cannot list libtest target `{target_name}` for {}: {}",
                        package.id, error.0
                    ))
                })?;
            let ignored =
                list_libtest_harness(&self.cargo_state, &harness, true).map_err(|error| {
                    TraceError(format!(
                        "cannot list ignored tests in target `{target_name}` for {}: {}",
                        package.id, error.0
                    ))
                })?;
            if !ignored.is_subset(&all) {
                return Err(TraceError(format!(
                    "libtest target `{target_name}` returned ignored names absent from its full list"
                )));
            }
            for name in all.difference(&ignored) {
                *registered.entry(name.clone()).or_insert(0) += 1;
            }
        }
        Ok(registered)
    }

    fn require_lane(&self, locator: &EvidenceLocator) -> Result<(), TraceError> {
        let mapped = match locator.kind {
            EvidenceKind::Rust | EvidenceKind::Parity => self
                .workflows
                .values()
                .filter(|workflow| workflow.has_required_ci_trigger())
                .flat_map(Workflow::run_steps)
                .any(|step| rust_lane_runs(&step, &locator.owner)),
            EvidenceKind::Swift => self
                .workflows
                .values()
                .filter(|workflow| workflow.has_required_ci_trigger())
                .flat_map(Workflow::run_steps)
                .any(|step| swift_lane_runs(&step, &locator.owner)),
            EvidenceKind::Kotlin => self
                .workflows
                .values()
                .filter(|workflow| workflow.has_required_ci_trigger())
                .flat_map(Workflow::run_steps)
                .any(|step| kotlin_lane_runs(&step, &locator.owner)),
            EvidenceKind::Script => self
                .workflows
                .values()
                .filter(|workflow| workflow.has_required_ci_trigger())
                .flat_map(Workflow::run_steps)
                .any(|step| script_lane_runs(&step, &locator.target)),
            EvidenceKind::Live => true,
        };
        if !mapped {
            return Err(TraceError(format!(
                "evidence {}:{}::{} does not map to a required deterministic CI lane",
                locator.kind.as_str(),
                locator.owner,
                locator.target
            )));
        }
        Ok(())
    }
}

impl EvidenceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Swift => "swift",
            Self::Kotlin => "kotlin",
            Self::Parity => "parity",
            Self::Script => "script",
            Self::Live => "live",
        }
    }
}

impl CargoState {
    fn new(repository_root: &Path) -> Result<Self, TraceError> {
        let directory = tempfile::Builder::new()
            .prefix("nmp-behavior-cargo-")
            .tempdir()
            .map_err(|error| {
                TraceError(format!(
                    "cannot create isolated Cargo evidence state: {error}"
                ))
            })?;
        let cargo_home = env::var_os("TRACE_CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| directory.path().join("home"));
        let cargo = PathBuf::from(env!("CARGO"));
        if !cargo.is_absolute() {
            return Err(TraceError(format!(
                "checker build supplied non-absolute Cargo executable {}",
                cargo.display()
            )));
        }
        let cargo_metadata = fs::symlink_metadata(&cargo).map_err(|error| {
            TraceError(format!(
                "cannot inspect checker-build Cargo executable {}: {error}",
                cargo.display()
            ))
        })?;
        if cargo_metadata.file_type().is_symlink()
            || !cargo_metadata.is_file()
            || !is_executable(&cargo_metadata)
        {
            return Err(TraceError(format!(
                "checker-build Cargo executable {} is not one executable regular file",
                cargo.display()
            )));
        }
        let cargo = fs::canonicalize(&cargo).map_err(|error| {
            TraceError(format!(
                "cannot canonicalize checker-build Cargo executable {}: {error}",
                cargo.display()
            ))
        })?;
        let target = directory.path().join("target");
        fs::create_dir_all(&cargo_home).map_err(|error| {
            TraceError(format!(
                "cannot create isolated Cargo home {}: {error}",
                cargo_home.display()
            ))
        })?;
        fs::create_dir_all(&target).map_err(|error| {
            TraceError(format!(
                "cannot create isolated Cargo target {}: {error}",
                target.display()
            ))
        })?;
        let canonical_home = fs::canonicalize(&cargo_home).map_err(|error| {
            TraceError(format!(
                "cannot canonicalize isolated Cargo home {}: {error}",
                cargo_home.display()
            ))
        })?;
        if canonical_home.starts_with(repository_root) {
            return Err(TraceError(format!(
                "isolated Cargo home {} is inside the repository",
                canonical_home.display()
            )));
        }
        Ok(Self {
            _directory: directory,
            cargo,
            cargo_home: canonical_home,
            target,
        })
    }
}

#[derive(Debug)]
struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn cargo_metadata(root: &Path, state: &CargoState) -> Result<Metadata, TraceError> {
    let mut command = cargo_command(state);
    command.args(["metadata", "--no-deps", "--format-version", "1"]);
    command.arg("--manifest-path").arg(root.join("Cargo.toml"));
    if root.join("Cargo.lock").is_file() {
        command.arg("--locked");
    }
    let output = run_bounded(
        command,
        "read pinned workspace Cargo metadata",
        Duration::from_secs(60),
        16 * 1024 * 1024,
    )?;
    if !output.status.success() {
        return Err(command_failure(
            "read pinned workspace Cargo metadata",
            &output,
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| TraceError(format!("cannot decode workspace Cargo metadata: {error}")))
}

fn cargo_command(state: &CargoState) -> Command {
    // Cargo injects this absolute tool path while compiling the detached
    // checker. Reusing it avoids PATH lookup and binds evidence compilation to
    // the same workflow-pinned toolchain that built the checker.
    let mut command = Command::new(&state.cargo);
    command
        .current_dir(state._directory.path())
        .env("CARGO_HOME", &state.cargo_home)
        .env("CARGO_TARGET_DIR", &state.target);
    #[cfg(test)]
    INJECT_GITHUB_CREDENTIALS.with(|inject| {
        if inject.get() {
            command
                .env("GH_TOKEN", "must-not-reach-untrusted-build-code")
                .env("GITHUB_TOKEN", "must-not-reach-untrusted-build-code");
        }
    });
    remove_github_credentials(&mut command);
    command
}

#[cfg(test)]
pub(crate) fn with_injected_github_credentials<T>(action: impl FnOnce() -> T) -> T {
    struct Reset;

    impl Drop for Reset {
        fn drop(&mut self) {
            INJECT_GITHUB_CREDENTIALS.with(|inject| inject.set(false));
        }
    }

    INJECT_GITHUB_CREDENTIALS.with(|inject| {
        assert!(
            !inject.replace(true),
            "credential injection cannot be nested"
        );
    });
    let _reset = Reset;
    action()
}

fn remove_github_credentials(command: &mut Command) {
    command.env_remove("GH_TOKEN").env_remove("GITHUB_TOKEN");
}

fn run_bounded(
    mut command: Command,
    label: &str,
    timeout: Duration,
    output_limit: u64,
) -> Result<BoundedOutput, TraceError> {
    let mut stdout = tempfile::tempfile()
        .map_err(|error| TraceError(format!("cannot capture {label} stdout: {error}")))?;
    let mut stderr = tempfile::tempfile()
        .map_err(|error| TraceError(format!("cannot capture {label} stderr: {error}")))?;
    command
        .stdout(Stdio::from(stdout.try_clone().map_err(|error| {
            TraceError(format!("cannot clone {label} stdout capture: {error}"))
        })?))
        .stderr(Stdio::from(stderr.try_clone().map_err(|error| {
            TraceError(format!("cannot clone {label} stderr capture: {error}"))
        })?));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| TraceError(format!("cannot start {label}: {error}")))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| TraceError(format!("cannot wait for {label}: {error}")))?
        {
            break status;
        }
        for (capture, stream) in [(&stdout, "stdout"), (&stderr, "stderr")] {
            let length = capture
                .metadata()
                .map_err(|error| {
                    TraceError(format!("cannot inspect {label} {stream} capture: {error}"))
                })?
                .len();
            if length > output_limit {
                terminate_process_group(&mut child);
                return Err(TraceError(format!(
                    "{label} {stream} exceeded {output_limit} bytes"
                )));
            }
        }
        if Instant::now() >= deadline {
            terminate_process_group(&mut child);
            return Err(TraceError(format!(
                "{label} exceeded {} milliseconds",
                timeout.as_millis()
            )));
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = read_bounded_capture(&mut stdout, output_limit, label, "stdout")?;
    let stderr = read_bounded_capture(&mut stderr, output_limit, label, "stderr")?;
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
    })
}

fn terminate_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let process_group = -(child.id() as i32);
        // The child is placed in its own process group immediately before
        // spawn, so this terminates Cargo plus build-script/proc-macro
        // descendants instead of leaving untrusted proof work behind.
        unsafe {
            libc::kill(process_group, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn read_bounded_capture(
    file: &mut fs::File,
    limit: u64,
    label: &str,
    stream: &str,
) -> Result<Vec<u8>, TraceError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| TraceError(format!("cannot seek {label} {stream}: {error}")))?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| TraceError(format!("cannot read {label} {stream}: {error}")))?;
    if bytes.len() as u64 > limit {
        return Err(TraceError(format!(
            "{label} {stream} exceeded {limit} bytes"
        )));
    }
    Ok(bytes)
}

fn command_failure(label: &str, output: &BoundedOutput) -> TraceError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    TraceError(format!(
        "{label} failed with {}: stderr=`{}` stdout=`{}`",
        output.status,
        stderr.trim(),
        stdout.trim()
    ))
}

fn read_manifest(path: &Path) -> Result<toml::Value, TraceError> {
    let source = fs::read_to_string(path).map_err(|error| {
        TraceError(format!(
            "cannot read Cargo manifest {}: {error}",
            path.display()
        ))
    })?;
    toml::from_str(&source).map_err(|error| {
        TraceError(format!(
            "cannot parse Cargo manifest {}: {error}",
            path.display()
        ))
    })
}

fn target_uses_custom_harness(manifest: &toml::Value, target: &Target) -> Result<bool, TraceError> {
    if target.kind.iter().any(|kind| {
        matches!(
            kind,
            TargetKind::CustomBuild | TargetKind::Unknown(_) | TargetKind::ProcMacro
        )
    }) {
        return Ok(true);
    }
    let table = manifest
        .as_table()
        .ok_or_else(|| TraceError("Cargo manifest root must be one TOML table".into()))?;
    if target.kind.iter().any(|kind| {
        matches!(
            kind,
            TargetKind::Lib
                | TargetKind::RLib
                | TargetKind::DyLib
                | TargetKind::CDyLib
                | TargetKind::StaticLib
        )
    }) {
        return Ok(table
            .get("lib")
            .and_then(toml::Value::as_table)
            .and_then(|target| target.get("harness"))
            .and_then(toml::Value::as_bool)
            == Some(false));
    }
    let key = if target.kind.contains(&TargetKind::Bin) {
        "bin"
    } else if target.kind.contains(&TargetKind::Test) {
        "test"
    } else if target.kind.contains(&TargetKind::Example) {
        "example"
    } else if target.kind.contains(&TargetKind::Bench) {
        "bench"
    } else {
        return Ok(true);
    };
    Ok(table
        .get(key)
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
        .find(|declared| {
            declared.get("name").and_then(toml::Value::as_str) == Some(target.name.as_str())
        })
        .and_then(|declared| declared.get("harness"))
        .and_then(toml::Value::as_bool)
        == Some(false))
}

fn list_libtest_harness(
    state: &CargoState,
    harness: &Path,
    ignored: bool,
) -> Result<BTreeSet<String>, TraceError> {
    let mut command = Command::new(harness);
    command
        .current_dir(state._directory.path())
        .args(["--list", "--format", "terse"]);
    if ignored {
        command.arg("--ignored");
    }
    remove_github_credentials(&mut command);
    let output = run_bounded(
        command,
        &format!("list libtest harness {}", harness.display()),
        Duration::from_secs(30),
        2 * 1024 * 1024,
    )?;
    if !output.status.success() {
        return Err(command_failure(
            &format!("list libtest harness {}", harness.display()),
            &output,
        ));
    }
    let stdout = std::str::from_utf8(&output.stdout).map_err(|error| {
        TraceError(format!(
            "libtest harness {} returned non-UTF-8 names: {error}",
            harness.display()
        ))
    })?;
    let mut names = BTreeSet::new();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let line = line.trim();
        if let Some(name) = line.strip_suffix(": test") {
            if name.is_empty() || !names.insert(name.to_owned()) {
                return Err(TraceError(format!(
                    "libtest harness {} returned an empty or duplicate test name",
                    harness.display()
                )));
            }
        } else if line.strip_suffix(": benchmark").is_none() {
            return Err(TraceError(format!(
                "libtest harness {} returned malformed list line `{line}`",
                harness.display()
            )));
        }
    }
    Ok(names)
}

fn unique_native_test(
    repository_root: &Path,
    root: &Path,
    extension: &str,
    _declaration: &str,
    target: &str,
    language: &str,
) -> Result<(), TraceError> {
    if !is_identifier(target) {
        return Err(TraceError(format!(
            "{language} evidence target `{target}` must be one exact function identifier"
        )));
    }
    let mut paths = Vec::new();
    collect_files(repository_root, root, extension, &mut paths)?;
    let mut matches = Vec::new();
    for path in paths {
        if language == "Kotlin"
            && !path
                .components()
                .any(|component| component.as_os_str() == "test")
        {
            continue;
        }
        let source = fs::read_to_string(&path).map_err(|error| {
            TraceError(format!(
                "cannot read {language} owner file {}: {error}",
                path.display()
            ))
        })?;
        let count = if language == "Swift" {
            swift_executable_test_count(&source, target)
        } else {
            kotlin_executable_test_count(&source, target)
        };
        if count > 0 {
            matches.extend(std::iter::repeat_n(path, count));
        }
    }
    if matches.len() != 1 {
        return Err(TraceError(format!(
            "{language} evidence target `{target}` must resolve uniquely to exactly one executable test; found {}",
            matches.len()
        )));
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct SwiftScope {
    xctest: bool,
    disabled: bool,
}

fn swift_executable_test_count(source: &str, target: &str) -> usize {
    let sanitized = strip_native_comments_and_strings(source);
    let needle = format!("func {target}(");
    let mut scopes: Vec<SwiftScope> = Vec::new();
    let mut pending_type_declaration: Option<SwiftScope> = None;
    let mut pending_test_attribute = None;
    let mut pending_declaration_disabled = false;
    let mut conditional_depth = 0usize;
    let mut count = 0;

    for line in sanitized.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#if") {
            conditional_depth += 1;
            continue;
        }
        if trimmed.starts_with("#endif") {
            conditional_depth = conditional_depth.saturating_sub(1);
            continue;
        }

        let annotations = annotation_names(trimmed);
        let annotation_disabled = (annotations.iter().any(|name| name == "Test")
            && trimmed.contains(".disabled"))
            || (annotations.iter().any(|name| name == "available")
                && trimmed.contains("unavailable"));
        pending_declaration_disabled |= annotation_disabled;

        if contains_swift_type_declaration(trimmed) {
            pending_type_declaration = Some(SwiftScope {
                xctest: trimmed.contains("XCTestCase"),
                disabled: pending_declaration_disabled || conditional_depth > 0,
            });
        } else if let Some(scope) = pending_type_declaration.as_mut() {
            scope.xctest |= trimmed.contains("XCTestCase");
            scope.disabled |= pending_declaration_disabled || conditional_depth > 0;
        }

        if annotations.iter().any(|name| name == "Test") {
            pending_test_attribute = Some(annotation_disabled);
        }
        if contains_declaration(trimmed, &needle) {
            let test_attribute_executes = pending_test_attribute == Some(false);
            let xctest_executes = target.starts_with("test")
                && scopes.iter().any(|scope| scope.xctest)
                && scopes.iter().all(|scope| !scope.disabled);
            if conditional_depth == 0
                && !pending_declaration_disabled
                && (test_attribute_executes || xctest_executes)
            {
                count += 1;
            }
            pending_test_attribute = None;
            pending_declaration_disabled = false;
        } else if !trimmed.is_empty()
            && annotations.is_empty()
            && !contains_swift_type_declaration(trimmed)
            && pending_type_declaration.is_none()
        {
            pending_test_attribute = None;
            pending_declaration_disabled = false;
        }

        for character in line.chars() {
            match character {
                '{' => {
                    scopes.push(pending_type_declaration.take().unwrap_or_default());
                    pending_declaration_disabled = false;
                }
                '}' => {
                    scopes.pop();
                }
                _ => {}
            }
        }
    }
    count
}

fn contains_swift_type_declaration(line: &str) -> bool {
    line.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .any(|token| matches!(token, "class" | "extension" | "struct"))
}

fn kotlin_executable_test_count(source: &str, target: &str) -> usize {
    let sanitized = strip_native_comments_and_strings(source);
    let needle = format!("fun {target}(");
    let mut scopes = Vec::new();
    let mut pending_type_disabled = None;
    let mut pending_annotations = Vec::new();
    let mut count = 0;

    for line in sanitized.lines() {
        let trimmed = line.trim();
        let annotations = annotation_names(trimmed);
        pending_annotations.extend(annotations);
        let disabled = pending_annotations
            .iter()
            .any(|name| matches!(name.as_str(), "Ignore" | "Disabled"));

        if contains_kotlin_type_declaration(trimmed) {
            pending_type_disabled = Some(disabled);
        }
        if contains_declaration(trimmed, &needle) {
            let test = pending_annotations.iter().any(|name| name == "Test");
            if test && !disabled && scopes.iter().all(|disabled| !disabled) {
                count += 1;
            }
            pending_annotations.clear();
        } else if !trimmed.is_empty()
            && annotation_names(trimmed).is_empty()
            && !contains_kotlin_type_declaration(trimmed)
            && pending_type_disabled.is_none()
        {
            pending_annotations.clear();
        }

        for character in line.chars() {
            match character {
                '{' => scopes.push(pending_type_disabled.take().unwrap_or(false)),
                '}' => {
                    scopes.pop();
                }
                _ => {}
            }
        }
    }
    count
}

fn contains_kotlin_type_declaration(line: &str) -> bool {
    line.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .any(|token| matches!(token, "class" | "object"))
}

fn annotation_names(line: &str) -> Vec<String> {
    let mut names = Vec::new();
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'@' {
            index += 1;
            continue;
        }
        index += 1;
        let start = index;
        while index < bytes.len()
            && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'.'))
        {
            index += 1;
        }
        if start == index {
            continue;
        }
        let path = &line[start..index];
        if let Some(name) = path.rsplit('.').next() {
            names.push(name.to_owned());
        }
    }
    names
}

fn contains_declaration(line: &str, needle: &str) -> bool {
    line.find(needle).is_some_and(|index| {
        index == 0
            || line[..index]
                .chars()
                .last()
                .is_some_and(|character| !character.is_ascii_alphanumeric() && character != '_')
    })
}

fn strip_native_comments_and_strings(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut characters = source.chars().peekable();
    let mut block_comment_depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;

    while let Some(character) = characters.next() {
        if block_comment_depth > 0 {
            if character == '/' && characters.peek() == Some(&'*') {
                characters.next();
                block_comment_depth += 1;
                output.push_str("  ");
            } else if character == '*' && characters.peek() == Some(&'/') {
                characters.next();
                block_comment_depth -= 1;
                output.push_str("  ");
            } else {
                output.push(if character == '\n' { '\n' } else { ' ' });
            }
            continue;
        }
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
            output.push(if character == '\n' { '\n' } else { ' ' });
            continue;
        }
        if character == '/' && characters.peek() == Some(&'/') {
            characters.next();
            output.push_str("  ");
            for rest in characters.by_ref() {
                if rest == '\n' {
                    output.push('\n');
                    break;
                }
                output.push(' ');
            }
        } else if character == '/' && characters.peek() == Some(&'*') {
            characters.next();
            block_comment_depth = 1;
            output.push_str("  ");
        } else if character == '"' {
            quoted = true;
            output.push(' ');
        } else {
            output.push(character);
        }
    }
    output
}

fn collect_files(
    repository_root: &Path,
    dir: &Path,
    extension: &str,
    paths: &mut Vec<PathBuf>,
) -> Result<(), TraceError> {
    repository_directory(repository_root, dir, "evidence source directory")?;
    for entry in fs::read_dir(dir)
        .map_err(|error| TraceError(format!("cannot enumerate {}: {error}", dir.display())))?
    {
        let entry = entry.map_err(|error| {
            TraceError(format!(
                "cannot enumerate entry under {}: {error}",
                dir.display()
            ))
        })?;
        let path = entry.path();
        let ty = entry
            .file_type()
            .map_err(|error| TraceError(format!("cannot inspect {}: {error}", path.display())))?;
        if ty.is_symlink() {
            return Err(TraceError(format!(
                "evidence source path {} is symlink-backed instead of repository-owned",
                path.display()
            )));
        }
        if ty.is_dir() && entry.file_name() != "target" && entry.file_name() != ".git" {
            collect_files(repository_root, &path, extension, paths)?;
        } else if ty.is_file() && path.extension().is_some_and(|value| value == extension) {
            repository_regular_file(repository_root, &path, "evidence source file")?;
            paths.push(path);
        }
    }
    Ok(())
}

fn load_workflows(root: &Path) -> Result<BTreeMap<String, Workflow>, TraceError> {
    let dir = root.join(".github/workflows");
    repository_directory(root, &dir, "workflow directory")?;
    let mut workflows = BTreeMap::new();
    for entry in fs::read_dir(&dir)
        .map_err(|error| TraceError(format!("cannot enumerate {}: {error}", dir.display())))?
    {
        let entry = entry.map_err(|error| {
            TraceError(format!(
                "cannot enumerate workflow under {}: {error}",
                dir.display()
            ))
        })?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| TraceError(format!("cannot inspect {}: {error}", path.display())))?;
        let is_yaml = path
            .extension()
            .is_some_and(|ext| ext == "yml" || ext == "yaml");
        if file_type.is_symlink() && is_yaml {
            return Err(TraceError(format!(
                "workflow {} is symlink-backed instead of repository-owned",
                path.display()
            )));
        }
        if !file_type.is_file() || !is_yaml {
            continue;
        }
        repository_regular_file(root, &path, "workflow")?;
        let stem = path
            .file_stem()
            .expect("workflow has file stem")
            .to_string_lossy()
            .into_owned();
        let source = fs::read_to_string(&path).map_err(|error| {
            TraceError(format!("cannot read workflow {}: {error}", path.display()))
        })?;
        let document = serde_yaml_ng::from_str(&source).map_err(|error| {
            TraceError(format!(
                "cannot parse workflow YAML {}: {error}",
                path.display()
            ))
        })?;
        workflows.insert(stem, Workflow { document });
    }
    Ok(workflows)
}

fn repository_directory(
    repository_root: &Path,
    path: &Path,
    label: &str,
) -> Result<(), TraceError> {
    repository_path_metadata(repository_root, path, label, true).map(|_| ())
}

fn repository_regular_file(
    repository_root: &Path,
    path: &Path,
    label: &str,
) -> Result<fs::Metadata, TraceError> {
    repository_path_metadata(repository_root, path, label, false)
}

fn repository_path_metadata(
    repository_root: &Path,
    path: &Path,
    label: &str,
    directory: bool,
) -> Result<fs::Metadata, TraceError> {
    let relative = path.strip_prefix(repository_root).map_err(|_| {
        TraceError(format!(
            "{label} {} is outside repository root {}",
            path.display(),
            repository_root.display()
        ))
    })?;
    let mut current = repository_root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    let mut final_metadata = None;
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            TraceError(format!(
                "{label} {} is unreadable: {error}",
                current.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(TraceError(format!(
                "{label} {} is symlink-backed instead of repository-owned",
                current.display()
            )));
        }
        let is_last = index + 1 == components.len();
        if !is_last && !metadata.is_dir() {
            return Err(TraceError(format!(
                "{label} parent {} is not a directory",
                current.display()
            )));
        }
        if is_last {
            final_metadata = Some(metadata);
        }
    }
    let metadata = final_metadata.unwrap_or_else(|| {
        fs::symlink_metadata(repository_root).expect("repository root was already readable")
    });
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(TraceError(format!(
            "{label} {} is not a repository-owned regular {}",
            path.display(),
            if directory { "directory" } else { "file" }
        )));
    }
    let canonical_root = fs::canonicalize(repository_root).map_err(|error| {
        TraceError(format!(
            "cannot canonicalize repository root {}: {error}",
            repository_root.display()
        ))
    })?;
    let canonical_path = fs::canonicalize(path).map_err(|error| {
        TraceError(format!(
            "cannot canonicalize {label} {}: {error}",
            path.display()
        ))
    })?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(TraceError(format!(
            "{label} {} escapes repository root {}",
            path.display(),
            repository_root.display()
        )));
    }
    Ok(metadata)
}

fn exact_child_directory(parent: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(parent)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() == name && entry.file_type().is_ok_and(|ty| ty.is_dir()))
        .map(|entry| entry.path())
}

impl Workflow {
    fn has_dispatch(&self) -> bool {
        self.has_trigger("workflow_dispatch")
    }

    fn has_required_ci_trigger(&self) -> bool {
        self.has_trigger("push") || self.has_trigger("pull_request")
    }

    fn has_trigger(&self, expected: &str) -> bool {
        let Some(root) = self.document.as_mapping() else {
            return false;
        };
        let Some(triggers) = mapping_get(root, "on").or_else(|| root.get(YamlValue::Bool(true)))
        else {
            return false;
        };
        match triggers {
            YamlValue::String(trigger) => trigger == expected,
            YamlValue::Sequence(triggers) => triggers
                .iter()
                .any(|trigger| trigger.as_str() == Some(expected)),
            YamlValue::Mapping(triggers) => mapping_get(triggers, expected).is_some(),
            _ => false,
        }
    }

    fn live_job_is_bounded_and_executable(&self, target: &str) -> bool {
        let Some(job) = self.job(target).and_then(YamlValue::as_mapping) else {
            return false;
        };
        if mapping_is_statically_disabled(job) || mapping_allows_failure(job) {
            return false;
        }
        let bounded = mapping_get(job, "timeout-minutes")
            .and_then(positive_integer)
            .is_some();
        bounded
            && mapping_get(job, "steps")
                .and_then(YamlValue::as_sequence)
                .is_some_and(|steps| {
                    steps.iter().any(|step| {
                        step.as_mapping().is_some_and(|step| {
                            !mapping_is_statically_disabled(step)
                                && !mapping_allows_failure(step)
                                && mapping_get(step, "run")
                                    .and_then(YamlValue::as_str)
                                    .is_some_and(command_has_executable_segment)
                        })
                    })
                })
    }

    fn run_steps(&self) -> Vec<WorkflowRunStep<'_>> {
        let Some(root) = self.document.as_mapping() else {
            return Vec::new();
        };
        let Some(jobs) = mapping_get(root, "jobs").and_then(YamlValue::as_mapping) else {
            return Vec::new();
        };
        let mut run_steps = Vec::new();
        for job in jobs.values().filter_map(YamlValue::as_mapping) {
            if mapping_is_statically_disabled(job) || mapping_allows_failure(job) {
                continue;
            }
            let Some(runner) = mapping_get(job, "runs-on").and_then(YamlValue::as_str) else {
                continue;
            };
            let Some(steps) = mapping_get(job, "steps").and_then(YamlValue::as_sequence) else {
                continue;
            };
            for step in steps.iter().filter_map(YamlValue::as_mapping) {
                if mapping_is_statically_disabled(step) || mapping_allows_failure(step) {
                    continue;
                }
                let Some(command) = mapping_get(step, "run").and_then(YamlValue::as_str) else {
                    continue;
                };
                run_steps.push(WorkflowRunStep {
                    command,
                    working_directory: mapping_get(step, "working-directory")
                        .and_then(YamlValue::as_str),
                    runner,
                    shell: mapping_get(step, "shell").and_then(YamlValue::as_str),
                });
            }
        }
        run_steps
    }

    fn job(&self, target: &str) -> Option<&YamlValue> {
        mapping_get(self.document.as_mapping()?, "jobs")
            .and_then(YamlValue::as_mapping)
            .and_then(|jobs| mapping_get(jobs, target))
    }
}

fn mapping_get<'a>(mapping: &'a YamlMapping, key: &str) -> Option<&'a YamlValue> {
    mapping.get(YamlValue::String(key.to_owned()))
}

fn mapping_is_statically_disabled(mapping: &YamlMapping) -> bool {
    mapping_get(mapping, "if").is_some_and(|condition| match condition {
        YamlValue::Bool(value) => !value,
        YamlValue::Number(number) => number.as_i64() == Some(0),
        YamlValue::String(value) => {
            let normalized = value
                .trim()
                .strip_prefix("${{")
                .and_then(|value| value.strip_suffix("}}"))
                .unwrap_or(value)
                .trim()
                .to_ascii_lowercase();
            normalized == "false" || normalized == "0"
        }
        _ => false,
    })
}

fn mapping_allows_failure(mapping: &YamlMapping) -> bool {
    mapping_get(mapping, "continue-on-error").is_some_and(|value| !yaml_falsey(value))
}

fn yaml_falsey(value: &YamlValue) -> bool {
    match value {
        YamlValue::Bool(value) => !*value,
        YamlValue::Number(number) => number.as_i64() == Some(0),
        YamlValue::String(value) => {
            let normalized = value
                .trim()
                .strip_prefix("${{")
                .and_then(|value| value.strip_suffix("}}"))
                .unwrap_or(value)
                .trim()
                .to_ascii_lowercase();
            normalized == "false" || normalized == "0"
        }
        _ => false,
    }
}

fn positive_integer(value: &YamlValue) -> Option<u64> {
    match value {
        YamlValue::Number(number) => number.as_u64().filter(|value| *value > 0),
        YamlValue::String(value) => value.parse().ok().filter(|value| *value > 0),
        _ => None,
    }
}

fn rust_lane_runs(step: &WorkflowRunStep<'_>, owner: &str) -> bool {
    let Some(command) = closed_proof_command(step) else {
        return false;
    };
    let Some((executable, arguments)) = executable_and_arguments(&command) else {
        return false;
    };
    if step.runner != "ubuntu-latest" || executable != "/home/runner/.cargo/bin/cargo" {
        return false;
    }
    arguments_equal(arguments, &["test", "--workspace"])
        || (arguments.len() == 3
            && arguments.first().is_some_and(|argument| argument == "test")
            && arguments
                .get(1)
                .is_some_and(|argument| argument == "-p" || argument == "--package")
            && arguments.get(2).is_some_and(|argument| argument == owner))
        || (arguments.len() == 2
            && arguments.first().is_some_and(|argument| argument == "test")
            && arguments
                .get(1)
                .is_some_and(|argument| argument == &format!("--package={owner}")))
}

fn swift_lane_runs(step: &WorkflowRunStep<'_>, owner: &str) -> bool {
    let Some(command) = closed_proof_command(step) else {
        return false;
    };
    let Some((executable, arguments)) = executable_and_arguments(&command) else {
        return false;
    };
    step.runner == "macos-14"
        && executable == "/usr/bin/xcrun"
        && ((working_directory_names_owner(step.working_directory, owner)
            && arguments_equal(arguments, &["--run", "swift", "test"]))
            || (step.working_directory.is_none()
                && arguments.len() == 5
                && arguments_equal(
                    &arguments[..4],
                    &["--run", "swift", "test", "--package-path"],
                )
                && arguments
                    .get(4)
                    .is_some_and(|path| path_names_owner(path, owner))))
}

fn kotlin_lane_runs(step: &WorkflowRunStep<'_>, owner: &str) -> bool {
    let Some(command) = closed_proof_command(step) else {
        return false;
    };
    let Some((executable, arguments)) = executable_and_arguments(&command) else {
        return false;
    };
    step.runner == "ubuntu-latest"
        && (arguments_equal(arguments, &["test"])
            || arguments_equal(arguments, &["test", "--console=plain"]))
        && ((executable == "./gradlew"
            && working_directory_names_owner(step.working_directory, owner))
            || (step.working_directory.is_none()
                && executable == format!("Packages/{owner}/gradlew")))
}

fn script_lane_runs(step: &WorkflowRunStep<'_>, target: &str) -> bool {
    let Some(command) = closed_proof_command(step) else {
        return false;
    };
    let Some((executable, arguments)) = executable_and_arguments(&command) else {
        return false;
    };
    (step.runner == "ubuntu-latest" || step.runner == "macos-14")
        && arguments.is_empty()
        && (executable == target || executable.strip_prefix("./") == Some(target))
}

fn arguments_equal(actual: &[String], expected: &[&str]) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual == expected)
}

fn working_directory_names_owner(directory: Option<&str>, owner: &str) -> bool {
    directory.is_some_and(|directory| directory == format!("Packages/{owner}"))
}

fn path_names_owner(path: &str, owner: &str) -> bool {
    path == format!("Packages/{owner}")
}

fn executable_name(executable: &str) -> &str {
    executable.rsplit('/').next().unwrap_or(executable)
}

fn executable_and_arguments(segment: &[String]) -> Option<(&str, &[String])> {
    let mut index = 0;
    if segment.first().is_some_and(|word| word == "env") {
        index += 1;
        while segment
            .get(index)
            .is_some_and(|word| word.starts_with('-') || is_environment_assignment(word))
        {
            index += 1;
        }
    } else {
        while segment
            .get(index)
            .is_some_and(|word| is_environment_assignment(word))
        {
            index += 1;
        }
    }
    let executable = segment.get(index)?.as_str();
    Some((executable, &segment[index + 1..]))
}

const CLOSED_PROOF_SHELL: &str = "/bin/bash --noprofile --norc -p -e -o pipefail {0}";

fn closed_proof_command(step: &WorkflowRunStep<'_>) -> Option<Vec<String>> {
    if step.shell != Some(CLOSED_PROOF_SHELL) {
        return None;
    }
    let command = step.command.trim();
    if command.is_empty()
        || command.contains('\n')
        || command.contains('\r')
        || !command.chars().all(is_closed_proof_character)
    {
        return None;
    }
    let words = command
        .split_ascii_whitespace()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    Some(words)
}

fn is_closed_proof_character(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || character.is_ascii_whitespace()
        || matches!(character, '_' | '-' | '.' | '/' | ':' | '+' | '@' | '=')
}

fn is_environment_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        let mut characters = name.chars();
        characters
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
            && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    })
}

fn command_has_executable_segment(command: &str) -> bool {
    !command_masks_or_statically_skips_proof(command)
        && shell_command_segments(command)
            .iter()
            .any(|segment| executable_and_arguments(segment).is_some())
}

fn command_masks_or_statically_skips_proof(command: &str) -> bool {
    if command.lines().any(|line| {
        let line = line.trim();
        line.contains('|')
            || line.contains('&')
            || line.starts_with("false &&")
            || line.starts_with("false&&")
            || line.starts_with('!')
            || line.contains("$(")
            || line.contains('`')
    }) {
        return true;
    }
    let segments = shell_command_segments(command);
    segments.iter().any(|segment| {
        executable_and_arguments(segment).is_some_and(|(executable, arguments)| {
            executable_name(executable) == "set"
                && (arguments.iter().any(|argument| argument == "+e")
                    || arguments.windows(2).any(|pair| pair == ["+o", "errexit"]))
        })
    })
}

fn shell_command_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut segment = Vec::new();
    let mut word = String::new();
    let mut characters = command.chars().peekable();
    let mut quote = None;
    let mut escaped = false;

    while let Some(character) = characters.next() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            } else {
                word.push(character);
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '#' if word.is_empty() => {
                for rest in characters.by_ref() {
                    if rest == '\n' {
                        finish_shell_segment(&mut word, &mut segment, &mut segments);
                        break;
                    }
                }
            }
            '\n' | ';' | '|' | '&' | '(' | ')' => {
                finish_shell_segment(&mut word, &mut segment, &mut segments);
            }
            character if character.is_whitespace() => {
                if !word.is_empty() {
                    segment.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(character),
        }
    }
    finish_shell_segment(&mut word, &mut segment, &mut segments);
    segments
}

fn finish_shell_segment(
    word: &mut String,
    segment: &mut Vec<String>,
    segments: &mut Vec<Vec<String>>,
) {
    if !word.is_empty() {
        segment.push(std::mem::take(word));
    }
    if !segment.is_empty() {
        segments.push(std::mem::take(segment));
    }
}

fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locator_grammar_is_uniform_and_property_is_not_a_kind() {
        assert_eq!(
            EvidenceLocator::parse("script:repository::scripts/check.sh")
                .unwrap()
                .owner,
            "repository"
        );
        assert!(EvidenceLocator::parse("script:scripts/check.sh").is_err());
        assert!(EvidenceLocator::parse("property:nmp::some_property").is_err());
        assert!(EvidenceLocator::parse("script:repository::check.sh").is_ok());
    }

    #[test]
    fn swift_proof_requires_xctestcase_or_test_attribute() {
        assert_eq!(swift_executable_test_count("func proof() {}\n", "proof"), 0);
        assert_eq!(
            swift_executable_test_count(
                "final class ProofTests:\n    XCTestCase\n{\n    func testProof() {}\n}\n",
                "testProof",
            ),
            1
        );
        assert_eq!(
            swift_executable_test_count("@Test(\"proof\")\nfunc proof() {}\n", "proof"),
            1
        );
        assert_eq!(
            swift_executable_test_count(
                "let fake = \"func proof() {}\"\n// @Test func proof() {}\n",
                "proof",
            ),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn untrusted_cargo_processes_have_active_time_and_output_bounds() {
        let mut output = Command::new("/bin/sh");
        output.args(["-c", "while :; do printf 1234567890; done"]);
        assert!(run_bounded(
            output,
            "unbounded output probe",
            Duration::from_secs(5),
            1024,
        )
        .unwrap_err()
        .0
        .contains("exceeded 1024 bytes"));

        let started = Instant::now();
        let mut timeout = Command::new("/bin/sh");
        timeout.args(["-c", "sleep 5"]);
        assert!(run_bounded(
            timeout,
            "unbounded time probe",
            Duration::from_millis(100),
            1024,
        )
        .unwrap_err()
        .0
        .contains("exceeded 100 milliseconds"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
