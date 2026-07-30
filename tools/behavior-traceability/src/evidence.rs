use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use cargo_metadata::MetadataCommand;
use syn::visit::{self, Visit};

use crate::model::TraceError;

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
    packages: BTreeMap<String, PathBuf>,
    workflows: BTreeMap<String, String>,
}

impl EvidenceResolver {
    pub(crate) fn new(root: &Path) -> Result<Self, TraceError> {
        let metadata = MetadataCommand::new()
            .current_dir(root)
            .no_deps()
            .exec()
            .map_err(|error| TraceError(format!("cannot read workspace package graph: {error}")))?;
        let packages = metadata
            .packages
            .into_iter()
            .map(|package| {
                let manifest = package.manifest_path.into_std_path_buf();
                (
                    package.name.to_string(),
                    manifest
                        .parent()
                        .expect("Cargo manifest has a parent")
                        .to_path_buf(),
                )
            })
            .collect();
        let workflows = load_workflows(root)?;
        Ok(Self {
            root: root.to_path_buf(),
            packages,
            workflows,
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
        let package_root = self.packages.get(&locator.owner).ok_or_else(|| {
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
        let mut paths = Vec::new();
        collect_files(package_root, "rs", &mut paths)?;
        let mut matches = Vec::new();
        let mut executable = Vec::new();
        for path in paths {
            let source = fs::read_to_string(&path).map_err(|error| {
                TraceError(format!(
                    "cannot read Rust owner file {}: {error}",
                    path.display()
                ))
            })?;
            let syntax = syn::parse_file(&source).map_err(|error| {
                TraceError(format!(
                    "cannot parse Rust owner file {}: {error}",
                    path.display()
                ))
            })?;
            let mut visitor = FunctionVisitor::new(&locator.target);
            visitor.visit_file(&syntax);
            if visitor.matches > 0 {
                matches.extend(std::iter::repeat_n(path.clone(), visitor.matches));
            }
            if visitor.executable > 0 {
                executable.extend(std::iter::repeat_n(path, visitor.executable));
            }
        }
        if matches.len() != 1 || executable.len() != 1 {
            return Err(TraceError(format!(
                "Rust evidence {}:{}::{} must resolve to exactly one test-attributed function; found {} same-named functions and {} executable proofs",
                match locator.kind {
                    EvidenceKind::Parity => "parity",
                    _ => "rust",
                },
                locator.owner,
                locator.target,
                matches.len(),
                executable.len()
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
        unique_native_test(&tests, "swift", "func", &locator.target, "Swift")
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
        let metadata = fs::metadata(&path).map_err(|error| {
            TraceError(format!(
                "script evidence target {} is unreadable: {error}",
                path.display()
            ))
        })?;
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
        if !has_yaml_key(workflow, "workflow_dispatch")
            || !live_job_is_bounded(workflow, &locator.target)
        {
            return Err(TraceError(format!(
                "live evidence `{}`::`{}` must name a job in a workflow_dispatch workflow with timeout-minutes",
                locator.owner, locator.target
            )));
        }
        Ok(())
    }

    fn require_lane(&self, locator: &EvidenceLocator) -> Result<(), TraceError> {
        let mapped = match locator.kind {
            EvidenceKind::Rust | EvidenceKind::Parity => self.workflows.values().any(|workflow| {
                workflow.contains("cargo test --workspace")
                    || workflow.contains(&format!("cargo test -p {}", locator.owner))
            }),
            EvidenceKind::Swift => self.workflows.values().any(|workflow| {
                workflow.contains("swift test") && workflow.contains(&locator.owner)
            }),
            EvidenceKind::Kotlin => self
                .workflows
                .values()
                .any(|workflow| workflow.contains("gradlew") && workflow.contains(&locator.owner)),
            EvidenceKind::Script => self
                .workflows
                .values()
                .any(|workflow| has_non_comment_line(workflow, &locator.target)),
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

struct FunctionVisitor<'a> {
    target: &'a str,
    matches: usize,
    executable: usize,
}

impl<'a> FunctionVisitor<'a> {
    fn new(target: &'a str) -> Self {
        Self {
            target,
            matches: 0,
            executable: 0,
        }
    }
}

impl<'ast> Visit<'ast> for FunctionVisitor<'_> {
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        if function.sig.ident == self.target {
            self.matches += 1;
            if function.attrs.iter().any(is_test_attribute) {
                self.executable += 1;
            }
        }
        visit::visit_item_fn(self, function);
    }

    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        if item
            .mac
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "proptest")
        {
            let tokens = item.mac.tokens.to_string();
            let needle = format!("fn {}", self.target);
            let count = tokens.match_indices(&needle).count();
            self.matches += count;
            self.executable += count;
            return;
        }
        if let Ok(file) = syn::parse2::<syn::File>(item.mac.tokens.clone()) {
            self.visit_file(&file);
        }
        visit::visit_item_macro(self, item);
    }
}

fn is_test_attribute(attribute: &syn::Attribute) -> bool {
    attribute
        .path()
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "test")
}

fn unique_native_test(
    root: &Path,
    extension: &str,
    declaration: &str,
    target: &str,
    language: &str,
) -> Result<(), TraceError> {
    if !is_identifier(target) {
        return Err(TraceError(format!(
            "{language} evidence target `{target}` must be one exact function identifier"
        )));
    }
    let mut paths = Vec::new();
    collect_files(root, extension, &mut paths)?;
    let needle = format!("{declaration} {target}(");
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
        let lines: Vec<_> = source.lines().collect();
        let count = lines
            .iter()
            .enumerate()
            .filter(|(index, line)| {
                if !line.trim().contains(&needle) {
                    return false;
                }
                language != "Kotlin"
                    || line.contains("@Test")
                    || lines[..*index]
                        .iter()
                        .rev()
                        .take_while(|line| {
                            let trimmed = line.trim();
                            trimmed.is_empty() || trimmed.starts_with('@')
                        })
                        .any(|line| line.contains("Test"))
            })
            .count();
        if count > 0 {
            matches.extend(std::iter::repeat_n(path, count));
        }
    }
    if matches.len() != 1 {
        return Err(TraceError(format!(
            "{language} evidence target `{target}` must resolve uniquely; found {} files",
            matches.len()
        )));
    }
    Ok(())
}

fn collect_files(dir: &Path, extension: &str, paths: &mut Vec<PathBuf>) -> Result<(), TraceError> {
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
        if ty.is_dir() && entry.file_name() != "target" && entry.file_name() != ".git" {
            collect_files(&path, extension, paths)?;
        } else if ty.is_file() && path.extension().is_some_and(|value| value == extension) {
            paths.push(path);
        }
    }
    Ok(())
}

fn load_workflows(root: &Path) -> Result<BTreeMap<String, String>, TraceError> {
    let dir = root.join(".github/workflows");
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
        if !path.is_file()
            || !path
                .extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
        {
            continue;
        }
        let stem = path
            .file_stem()
            .expect("workflow has file stem")
            .to_string_lossy()
            .into_owned();
        let source = fs::read_to_string(&path).map_err(|error| {
            TraceError(format!("cannot read workflow {}: {error}", path.display()))
        })?;
        workflows.insert(stem, source);
    }
    Ok(workflows)
}

fn exact_child_directory(parent: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(parent)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() == name && entry.file_type().is_ok_and(|ty| ty.is_dir()))
        .map(|entry| entry.path())
}

fn has_yaml_key(workflow: &str, key: &str) -> bool {
    let key = format!("{key}:");
    workflow
        .lines()
        .map(str::trim)
        .any(|line| !line.starts_with('#') && line.starts_with(&key))
}

fn live_job_is_bounded(workflow: &str, target: &str) -> bool {
    let lines: Vec<_> = workflow.lines().collect();
    let target_key = format!("{target}:");
    let Some(start) = lines
        .iter()
        .position(|line| line.len() - line.trim_start().len() == 2 && line.trim() == target_key)
    else {
        return false;
    };
    lines[start + 1..]
        .iter()
        .take_while(|line| {
            let trimmed = line.trim();
            trimmed.is_empty()
                || trimmed.starts_with('#')
                || line.len() - line.trim_start().len() > 2
        })
        .any(|line| line.trim_start().starts_with("timeout-minutes:"))
}

fn has_non_comment_line(workflow: &str, needle: &str) -> bool {
    workflow
        .lines()
        .map(str::trim)
        .any(|line| !line.starts_with('#') && line.contains(needle))
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
    fn executable_attribute_is_distinct_from_same_named_production_function() {
        let syntax = syn::parse_file(
            r#"
fn proof() {}
mod tests {
    #[test]
    fn proof() {}
}
"#,
        )
        .unwrap();
        let mut visitor = FunctionVisitor::new("proof");
        visitor.visit_file(&syntax);
        assert_eq!(visitor.matches, 2);
        assert_eq!(visitor.executable, 1);
    }

    #[test]
    fn rust_property_proofs_remain_executable_rust_locators() {
        let syntax = syn::parse_file(
            r#"
proptest! {
    #[test]
    fn property_proof(value in 0usize..10) {
        prop_assert!(value < 10);
    }
}
"#,
        )
        .unwrap();
        let mut visitor = FunctionVisitor::new("property_proof");
        visitor.visit_file(&syntax);
        assert_eq!(visitor.matches, 1);
        assert_eq!(visitor.executable, 1);
    }
}
