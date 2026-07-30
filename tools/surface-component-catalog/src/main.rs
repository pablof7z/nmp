use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsStr,
    io::{self, Write},
    path::{Component, Path},
    process::Command,
};

const CATALOG_ROOT: &str = "docs/surface/components";
const CATALOG_README: &str = "docs/surface/components/README.md";
const LEGACY_SNAPSHOT: &str = "docs/surface/nmp-ffi-component.txt";
const RUST_FACADE_SNAPSHOT: &str = "docs/surface/nmp-facade.txt";
const ALLOWLIST: &str = "scripts/check-sdk-parity-allowlist.toml";
const MAX_DESCRIPTOR_BYTES: usize = 32_768;
const MAX_SNAPSHOT_LINES: usize = 20_000;
const MAX_SNAPSHOT_BYTES: usize = 2_000_000;
const MAX_RECORDS: usize = 128;

type Error = Box<dyn std::error::Error>;
type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum State {
    Active,
    Retired,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Android {
    gradle_project: String,
    namespace: String,
    maven_coordinate: String,
    manifests: Vec<String>,
    sources: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    schema: u32,
    key: String,
    state: State,
    uniffi_namespace: String,
    artifact_owner: String,

    ffi_package: Option<String>,
    ffi_manifest: Option<String>,
    library_stem: Option<String>,
    ffi_sources: Option<Vec<String>>,

    swift_manifests: Option<Vec<String>>,
    swift_sources: Option<Vec<String>>,
    swift_omission_reason: Option<String>,
    kotlin_manifests: Option<Vec<String>>,
    kotlin_sources: Option<Vec<String>>,
    kotlin_omission_reason: Option<String>,

    android: Option<Android>,

    reserved_ffi_package: Option<String>,
    reserved_library_stem: Option<String>,
    reserved_android_gradle_project: Option<String>,
    reserved_android_namespace: Option<String>,
    reserved_android_maven_coordinate: Option<String>,
    retired_by_pr: Option<u64>,
    retired_by_url: Option<String>,
    removed_paths: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
struct Entry {
    mode: String,
    kind: String,
    oid: String,
    path: String,
}

#[derive(Clone, Debug)]
struct Record {
    descriptor: Descriptor,
    bytes: Vec<u8>,
    entry: Entry,
}

#[derive(Debug)]
struct Catalog {
    records: BTreeMap<String, Record>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Allowlist {
    schema: u32,
    #[serde(default)]
    exception: Vec<Exception>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Exception {
    component: String,
    concept: String,
    platform: String,
    justification: String,
}

fn invalid(message: impl Into<String>) -> Error {
    io::Error::other(message.into()).into()
}

fn usage() -> Error {
    invalid(
        "usage: nmp-surface-component-catalog \
         <validate|transition|projections|active-rows|parity-rows|allowlist-rows|render-tombstone> ...",
    )
}

fn git(repo: &Path, args: &[&OsStr]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(invalid(format!(
            "git {} failed: {}",
            args.iter()
                .map(|value| value.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn commit_exists(repo: &Path, reference: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "-e", &format!("{reference}^{{commit}}")])
        .status()
        .is_ok_and(|status| status.success())
}

fn parse_ls_tree(bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for raw in bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let Some(tab) = raw.iter().position(|byte| *byte == b'\t') else {
            return Err(invalid("git ls-tree row has no path separator"));
        };
        let header = std::str::from_utf8(&raw[..tab])
            .map_err(|_| invalid("git ls-tree header was not UTF-8"))?;
        let path =
            std::str::from_utf8(&raw[tab + 1..]).map_err(|_| invalid("git path was not UTF-8"))?;
        let mut parts = header.split(' ');
        let mode = parts.next().ok_or_else(|| invalid("missing tree mode"))?;
        let kind = parts.next().ok_or_else(|| invalid("missing tree kind"))?;
        let oid = parts
            .next()
            .ok_or_else(|| invalid("missing tree object id"))?;
        if parts.next().is_some() {
            return Err(invalid("unexpected git ls-tree header fields"));
        }
        entries.push(Entry {
            mode: mode.to_owned(),
            kind: kind.to_owned(),
            oid: oid.to_owned(),
            path: path.to_owned(),
        });
    }
    Ok(entries)
}

fn tree_entries(repo: &Path, reference: &str, root: &str) -> Result<Vec<Entry>> {
    let bytes = git(
        repo,
        &[
            OsStr::new("ls-tree"),
            OsStr::new("-r"),
            OsStr::new("-z"),
            OsStr::new(reference),
            OsStr::new("--"),
            OsStr::new(root),
        ],
    )?;
    parse_ls_tree(&bytes)
}

fn exact_entry(repo: &Path, reference: &str, path: &str) -> Result<Option<Entry>> {
    let bytes = git(
        repo,
        &[
            OsStr::new("ls-tree"),
            OsStr::new("-z"),
            OsStr::new(reference),
            OsStr::new("--"),
            OsStr::new(path),
        ],
    )?;
    let mut entries = parse_ls_tree(&bytes)?;
    entries.retain(|entry| entry.path == path);
    match entries.len() {
        0 => Ok(None),
        1 => Ok(entries.pop()),
        _ => Err(invalid(format!(
            "multiple tree entries resolve exact path {path}"
        ))),
    }
}

fn blob(repo: &Path, oid: &str) -> Result<Vec<u8>> {
    git(
        repo,
        &[OsStr::new("cat-file"), OsStr::new("blob"), OsStr::new(oid)],
    )
}

fn regular_blob(entry: &Entry) -> bool {
    entry.kind == "blob" && matches!(entry.mode.as_str(), "100644" | "100755")
}

fn require_regular_file(repo: &Path, reference: &str, path: &str) -> Result<()> {
    let entry = exact_entry(repo, reference, path)?
        .ok_or_else(|| invalid(format!("declared file is absent at {reference}: {path}")))?;
    if !regular_blob(&entry) {
        return Err(invalid(format!(
            "declared file is not a regular blob at {reference}: {path} ({}/{})",
            entry.mode, entry.kind
        )));
    }
    Ok(())
}

fn require_tree(repo: &Path, reference: &str, path: &str) -> Result<()> {
    let entry = exact_entry(repo, reference, path)?.ok_or_else(|| {
        invalid(format!(
            "declared source root is absent at {reference}: {path}"
        ))
    })?;
    if entry.kind != "tree" || entry.mode != "040000" {
        return Err(invalid(format!(
            "declared source root is not a tree at {reference}: {path} ({}/{})",
            entry.mode, entry.kind
        )));
    }
    Ok(())
}

fn validate_relative_path(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.contains('\\')
        || value.contains('\0')
        || value.contains('\n')
        || value.contains('\r')
        || value.contains('\t')
    {
        return Err(invalid(format!(
            "{label} is not a canonical repository path: {value:?}"
        )));
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(invalid(format!("{label} must be relative: {value}")));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(invalid(format!(
                "{label} contains a non-canonical path component: {value}"
            )));
        }
    }
    if path.as_os_str().to_string_lossy() != value {
        return Err(invalid(format!(
            "{label} is not canonically spelled: {value}"
        )));
    }
    Ok(())
}

fn validate_key(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('-')
        || value.ends_with('-')
        || value.split('-').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
    {
        return Err(invalid(format!(
            "{label} is not canonical kebab-case: {value}"
        )));
    }
    Ok(())
}

fn validate_rust_ident(value: &str, label: &str) -> Result<()> {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return Err(invalid(format!("{label} is empty")));
    };
    if !first.is_ascii_lowercase()
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(invalid(format!(
            "{label} is not a canonical Rust identifier: {value}"
        )));
    }
    Ok(())
}

fn nonempty(value: &str, label: &str) -> Result<()> {
    if value.trim().is_empty()
        || value.contains('\0')
        || value.contains('\n')
        || value.contains('\r')
        || value.contains('\t')
    {
        return Err(invalid(format!(
            "{label} must be one non-empty reviewable line"
        )));
    }
    Ok(())
}

fn required_vec<'a>(value: &'a Option<Vec<String>>, label: &str) -> Result<&'a Vec<String>> {
    value
        .as_ref()
        .ok_or_else(|| invalid(format!("active descriptor is missing {label}")))
}

fn validate_paths(values: &[String], label: &str) -> Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        validate_relative_path(value, label)?;
        if !seen.insert(value) {
            return Err(invalid(format!("duplicate {label}: {value}")));
        }
    }
    Ok(())
}

fn validate_platform(
    manifests: &Option<Vec<String>>,
    sources: &Option<Vec<String>>,
    omission: &Option<String>,
    platform: &str,
) -> Result<()> {
    let manifests = required_vec(manifests, &format!("{platform}_manifests"))?;
    let sources = required_vec(sources, &format!("{platform}_sources"))?;
    validate_paths(manifests, &format!("{platform} manifest"))?;
    validate_paths(sources, &format!("{platform} source"))?;
    match (manifests.is_empty(), sources.is_empty(), omission) {
        (true, true, Some(reason)) => nonempty(reason, &format!("{platform}_omission_reason")),
        (true, true, None) => Err(invalid(format!(
            "{platform} roots are empty without {platform}_omission_reason"
        ))),
        (false, false, None) => Ok(()),
        (false, false, Some(_)) => Err(invalid(format!(
            "{platform}_omission_reason is forbidden when {platform} roots are declared"
        ))),
        _ => Err(invalid(format!(
            "{platform} manifests and sources must both be populated or both be empty"
        ))),
    }
}

fn active_removed_paths(descriptor: &Descriptor) -> Result<Vec<String>> {
    let mut paths = BTreeSet::new();
    if let Some(path) = &descriptor.ffi_manifest {
        paths.insert(path.clone());
    }
    for values in [
        &descriptor.ffi_sources,
        &descriptor.swift_manifests,
        &descriptor.swift_sources,
        &descriptor.kotlin_manifests,
        &descriptor.kotlin_sources,
    ] {
        for path in values
            .as_ref()
            .ok_or_else(|| invalid("active descriptor path fields are incomplete"))?
        {
            paths.insert(path.clone());
        }
    }
    if let Some(android) = &descriptor.android {
        paths.extend(android.manifests.iter().cloned());
        paths.extend(android.sources.iter().cloned());
    }
    if paths.is_empty() {
        return Err(invalid(
            "active descriptor derives an empty retirement path set",
        ));
    }
    Ok(paths.into_iter().collect())
}

fn validate_active_shape(descriptor: &Descriptor) -> Result<()> {
    let ffi_sources = required_vec(&descriptor.ffi_sources, "ffi_sources")?;
    if ffi_sources.is_empty() {
        return Err(invalid("active descriptor ffi_sources must not be empty"));
    }
    validate_paths(ffi_sources, "ffi source")?;
    validate_platform(
        &descriptor.swift_manifests,
        &descriptor.swift_sources,
        &descriptor.swift_omission_reason,
        "swift",
    )?;
    validate_platform(
        &descriptor.kotlin_manifests,
        &descriptor.kotlin_sources,
        &descriptor.kotlin_omission_reason,
        "kotlin",
    )?;

    for (name, present) in [
        (
            "reserved_ffi_package",
            descriptor.reserved_ffi_package.is_some(),
        ),
        (
            "reserved_library_stem",
            descriptor.reserved_library_stem.is_some(),
        ),
        (
            "reserved_android_gradle_project",
            descriptor.reserved_android_gradle_project.is_some(),
        ),
        (
            "reserved_android_namespace",
            descriptor.reserved_android_namespace.is_some(),
        ),
        (
            "reserved_android_maven_coordinate",
            descriptor.reserved_android_maven_coordinate.is_some(),
        ),
        ("retired_by_pr", descriptor.retired_by_pr.is_some()),
        ("retired_by_url", descriptor.retired_by_url.is_some()),
        ("removed_paths", descriptor.removed_paths.is_some()),
    ] {
        if present {
            return Err(invalid(format!(
                "active descriptor contains retired-only field {name}"
            )));
        }
    }

    let self_owned = descriptor.artifact_owner == descriptor.key;
    match (
        &descriptor.ffi_package,
        &descriptor.ffi_manifest,
        &descriptor.library_stem,
    ) {
        (Some(package), Some(manifest), Some(stem)) if self_owned => {
            validate_key(package, "ffi_package")?;
            validate_relative_path(manifest, "ffi_manifest")?;
            validate_rust_ident(stem, "library_stem")?;
        }
        (None, None, None) if !self_owned => {}
        (Some(_), Some(_), Some(_)) => {
            return Err(invalid(
                "co-located component must not declare ffi_package, ffi_manifest, or library_stem",
            ))
        }
        (None, None, None) => {
            return Err(invalid(
                "self-owning component must declare ffi_package, ffi_manifest, and library_stem",
            ))
        }
        _ => {
            return Err(invalid(
                "ffi_package, ffi_manifest, and library_stem must be declared as one group",
            ))
        }
    }

    if let Some(android) = &descriptor.android {
        nonempty(&android.gradle_project, "android.gradle_project")?;
        if !android.gradle_project.starts_with(':')
            || android.gradle_project.contains(char::is_whitespace)
        {
            return Err(invalid(
                "android.gradle_project must be a canonical Gradle project path",
            ));
        }
        nonempty(&android.namespace, "android.namespace")?;
        nonempty(&android.maven_coordinate, "android.maven_coordinate")?;
        if android.maven_coordinate.split(':').count() != 2 {
            return Err(invalid(
                "android.maven_coordinate must contain exactly group:artifact",
            ));
        }
        if android.manifests.is_empty() || android.sources.is_empty() {
            return Err(invalid(
                "android manifests and sources must both be non-empty",
            ));
        }
        validate_paths(&android.manifests, "android manifest")?;
        validate_paths(&android.sources, "android source")?;
    }
    Ok(())
}

fn validate_retired_shape(descriptor: &Descriptor) -> Result<()> {
    for (name, present) in [
        ("ffi_package", descriptor.ffi_package.is_some()),
        ("ffi_manifest", descriptor.ffi_manifest.is_some()),
        ("library_stem", descriptor.library_stem.is_some()),
        ("ffi_sources", descriptor.ffi_sources.is_some()),
        ("swift_manifests", descriptor.swift_manifests.is_some()),
        ("swift_sources", descriptor.swift_sources.is_some()),
        (
            "swift_omission_reason",
            descriptor.swift_omission_reason.is_some(),
        ),
        ("kotlin_manifests", descriptor.kotlin_manifests.is_some()),
        ("kotlin_sources", descriptor.kotlin_sources.is_some()),
        (
            "kotlin_omission_reason",
            descriptor.kotlin_omission_reason.is_some(),
        ),
        ("android", descriptor.android.is_some()),
    ] {
        if present {
            return Err(invalid(format!(
                "retired descriptor contains active-only field {name}"
            )));
        }
    }
    let pr = descriptor
        .retired_by_pr
        .ok_or_else(|| invalid("retired descriptor is missing retired_by_pr"))?;
    if pr == 0 {
        return Err(invalid("retired_by_pr must be positive"));
    }
    let url = descriptor
        .retired_by_url
        .as_deref()
        .ok_or_else(|| invalid("retired descriptor is missing retired_by_url"))?;
    if url != format!("https://github.com/pablof7z/nmp/pull/{pr}") {
        return Err(invalid("retired_by_url must match retired_by_pr exactly"));
    }
    let removed = descriptor
        .removed_paths
        .as_ref()
        .ok_or_else(|| invalid("retired descriptor is missing removed_paths"))?;
    if removed.is_empty() {
        return Err(invalid(
            "retired descriptor removed_paths must not be empty",
        ));
    }
    validate_paths(removed, "removed path")?;
    if removed.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(invalid(
            "retired descriptor removed_paths must be sorted and unique",
        ));
    }

    let self_owned = descriptor.artifact_owner == descriptor.key;
    match (
        &descriptor.reserved_ffi_package,
        &descriptor.reserved_library_stem,
    ) {
        (Some(package), Some(stem)) if self_owned => {
            validate_key(package, "reserved_ffi_package")?;
            validate_rust_ident(stem, "reserved_library_stem")?;
        }
        (None, None) if !self_owned => {}
        (Some(_), Some(_)) => {
            return Err(invalid(
                "co-located tombstone must not reserve artifact-owner library fields",
            ))
        }
        (None, None) => {
            return Err(invalid(
                "self-owning tombstone must reserve ffi package and library identities",
            ))
        }
        _ => {
            return Err(invalid(
                "reserved_ffi_package and reserved_library_stem must be declared together",
            ))
        }
    }
    let android_reserved = [
        descriptor.reserved_android_gradle_project.as_ref(),
        descriptor.reserved_android_namespace.as_ref(),
        descriptor.reserved_android_maven_coordinate.as_ref(),
    ];
    if android_reserved.iter().any(|value| value.is_some())
        && android_reserved.iter().any(|value| value.is_none())
    {
        return Err(invalid(
            "retired Android identities must be reserved as one complete group",
        ));
    }
    for (label, value) in [
        (
            "reserved_android_gradle_project",
            &descriptor.reserved_android_gradle_project,
        ),
        (
            "reserved_android_namespace",
            &descriptor.reserved_android_namespace,
        ),
        (
            "reserved_android_maven_coordinate",
            &descriptor.reserved_android_maven_coordinate,
        ),
    ] {
        if let Some(value) = value {
            nonempty(value, label)?;
        }
    }
    Ok(())
}

fn check_declared_paths(repo: &Path, reference: &str, descriptor: &Descriptor) -> Result<()> {
    if descriptor.state == State::Retired {
        for path in descriptor
            .removed_paths
            .as_ref()
            .ok_or_else(|| invalid("retired removed_paths missing after validation"))?
        {
            if exact_entry(repo, reference, path)?.is_some() {
                return Err(invalid(format!(
                    "retired component {} path was resurrected: {path}",
                    descriptor.key
                )));
            }
        }
        return Ok(());
    }
    if let Some(manifest) = &descriptor.ffi_manifest {
        require_regular_file(repo, reference, manifest)?;
    }
    for source in required_vec(&descriptor.ffi_sources, "ffi_sources")? {
        require_tree(repo, reference, source)?;
    }
    for manifest in required_vec(&descriptor.swift_manifests, "swift_manifests")? {
        require_regular_file(repo, reference, manifest)?;
    }
    for source in required_vec(&descriptor.swift_sources, "swift_sources")? {
        require_tree(repo, reference, source)?;
    }
    for manifest in required_vec(&descriptor.kotlin_manifests, "kotlin_manifests")? {
        require_regular_file(repo, reference, manifest)?;
    }
    for source in required_vec(&descriptor.kotlin_sources, "kotlin_sources")? {
        require_tree(repo, reference, source)?;
    }
    if let Some(android) = &descriptor.android {
        for manifest in &android.manifests {
            require_regular_file(repo, reference, manifest)?;
        }
        for source in &android.sources {
            require_tree(repo, reference, source)?;
        }
    }
    Ok(())
}

fn descriptor_path(key: &str) -> String {
    format!("{CATALOG_ROOT}/{key}/component.toml")
}

fn snapshot_path(key: &str) -> String {
    format!("{CATALOG_ROOT}/{key}/uniffi.txt")
}

fn validate_snapshot(key: &str, entry: &Entry, bytes: &[u8]) -> Result<()> {
    if !regular_blob(entry) || entry.mode != "100644" {
        return Err(invalid(format!(
            "{key} snapshot is not a regular file ({}/{})",
            entry.mode, entry.kind
        )));
    }
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(invalid(format!(
            "{key} snapshot exceeds {MAX_SNAPSHOT_BYTES} bytes: {}",
            bytes.len()
        )));
    }
    if bytes.contains(&0) {
        return Err(invalid(format!("{key} snapshot contains a NUL byte")));
    }
    if bytes.contains(&b'\r') {
        return Err(invalid(format!(
            "{key} snapshot contains a carriage return; snapshots must use LF line endings"
        )));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid(format!("{key} snapshot is not valid UTF-8")))?;
    let lines = text.lines().count();
    if lines > MAX_SNAPSHOT_LINES {
        return Err(invalid(format!(
            "{key} snapshot exceeds {MAX_SNAPSHOT_LINES} lines: {lines}"
        )));
    }
    for noise in [
        "CfgTrace",
        "CfgAttrTrace",
        "#[attr = Inline",
        "/Users/",
        "/home/",
        "/private/var/",
        ".cargo/registry",
    ] {
        if text.contains(noise) {
            return Err(invalid(format!(
                "{key} snapshot contains compiler noise or an absolute path: {noise}"
            )));
        }
    }
    Ok(())
}

fn overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn insert_unique(
    identities: &mut BTreeMap<String, String>,
    identity: String,
    key: &str,
    label: &str,
) -> Result<()> {
    if let Some(existing) = identities.insert(identity.clone(), key.to_owned()) {
        return Err(invalid(format!(
            "duplicate {label} {identity:?} in {existing} and {key}"
        )));
    }
    Ok(())
}

fn validate_catalog(repo: &Path, reference: &str) -> Result<Catalog> {
    if !commit_exists(repo, reference) {
        return Err(invalid(format!("commit is unavailable: {reference}")));
    }
    if exact_entry(repo, reference, LEGACY_SNAPSHOT)?.is_some() {
        return Err(invalid(format!(
            "legacy snapshot path is forbidden: {LEGACY_SNAPSHOT}"
        )));
    }
    require_regular_file(repo, reference, CATALOG_README)?;
    let entries = tree_entries(repo, reference, CATALOG_ROOT)?;
    let entry_map = entries
        .iter()
        .cloned()
        .map(|entry| (entry.path.clone(), entry))
        .collect::<BTreeMap<_, _>>();

    let mut records = BTreeMap::new();
    for entry in &entries {
        if !entry.path.ends_with("/component.toml") {
            continue;
        }
        if !regular_blob(entry) || entry.mode != "100644" {
            return Err(invalid(format!(
                "component descriptor is not a regular 0644 file: {} ({}/{})",
                entry.path, entry.mode, entry.kind
            )));
        }
        let bytes = blob(repo, &entry.oid)?;
        if bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Err(invalid(format!(
                "component descriptor exceeds {MAX_DESCRIPTOR_BYTES} bytes: {} ({})",
                entry.path,
                bytes.len()
            )));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| invalid(format!("descriptor is not UTF-8: {}", entry.path)))?;
        let descriptor: Descriptor =
            toml::from_str(text).map_err(|error| invalid(format!("{}: {error}", entry.path)))?;
        if descriptor.schema != 1 {
            return Err(invalid(format!(
                "{}: unsupported schema {}",
                entry.path, descriptor.schema
            )));
        }
        validate_key(&descriptor.key, "component key")?;
        validate_key(&descriptor.artifact_owner, "artifact_owner")?;
        validate_rust_ident(&descriptor.uniffi_namespace, "uniffi_namespace")?;
        let expected = descriptor_path(&descriptor.key);
        if entry.path != expected {
            return Err(invalid(format!(
                "descriptor path/key mismatch: {} must be {expected}",
                entry.path
            )));
        }
        match descriptor.state {
            State::Active => validate_active_shape(&descriptor)?,
            State::Retired => validate_retired_shape(&descriptor)?,
        }
        check_declared_paths(repo, reference, &descriptor)?;
        let key = descriptor.key.clone();
        if records
            .insert(
                key.clone(),
                Record {
                    descriptor,
                    bytes,
                    entry: entry.clone(),
                },
            )
            .is_some()
        {
            return Err(invalid(format!("duplicate component key: {key}")));
        }
    }
    if records.is_empty() {
        return Err(invalid("component catalog contains no descriptors"));
    }
    if records.len() > MAX_RECORDS {
        return Err(invalid(format!(
            "component catalog exceeds {MAX_RECORDS} records: {}",
            records.len()
        )));
    }

    let mut expected_inventory = BTreeSet::from([CATALOG_README.to_owned()]);
    for (key, record) in &records {
        expected_inventory.insert(descriptor_path(key));
        let snapshot = snapshot_path(key);
        match record.descriptor.state {
            State::Active => {
                expected_inventory.insert(snapshot.clone());
                let entry = entry_map.get(&snapshot).ok_or_else(|| {
                    invalid(format!("active component snapshot is missing: {snapshot}"))
                })?;
                validate_snapshot(key, entry, &blob(repo, &entry.oid)?)?;
            }
            State::Retired => {
                if entry_map.contains_key(&snapshot) {
                    return Err(invalid(format!(
                        "retired component retains forbidden snapshot: {snapshot}"
                    )));
                }
            }
        }
    }
    let actual_inventory = entry_map.keys().cloned().collect::<BTreeSet<_>>();
    if actual_inventory != expected_inventory {
        let extra = actual_inventory
            .difference(&expected_inventory)
            .cloned()
            .collect::<Vec<_>>();
        let missing = expected_inventory
            .difference(&actual_inventory)
            .cloned()
            .collect::<Vec<_>>();
        return Err(invalid(format!(
            "component inventory mismatch; missing={missing:?} extra/orphan={extra:?}"
        )));
    }

    let mut namespaces = BTreeMap::new();
    let mut packages = BTreeMap::new();
    let mut libraries = BTreeMap::new();
    let mut android_projects = BTreeMap::new();
    let mut android_namespaces = BTreeMap::new();
    let mut android_coordinates = BTreeMap::new();
    let mut roots: Vec<(String, String)> = Vec::new();
    for (key, record) in &records {
        let descriptor = &record.descriptor;
        insert_unique(
            &mut namespaces,
            descriptor.uniffi_namespace.clone(),
            key,
            "UniFFI namespace",
        )?;
        match descriptor.state {
            State::Active => {
                if let Some(package) = &descriptor.ffi_package {
                    insert_unique(&mut packages, package.clone(), key, "FFI package")?;
                }
                if let Some(library) = &descriptor.library_stem {
                    insert_unique(&mut libraries, library.clone(), key, "library stem")?;
                }
                for source in descriptor
                    .swift_sources
                    .as_ref()
                    .into_iter()
                    .flatten()
                    .chain(descriptor.kotlin_sources.as_ref().into_iter().flatten())
                    .chain(descriptor.ffi_sources.as_ref().into_iter().flatten())
                {
                    for (other, owner) in &roots {
                        if overlap(source, other) {
                            return Err(invalid(format!(
                                "overlapping source roots in {owner} and {key}: {other} / {source}"
                            )));
                        }
                    }
                    roots.push((source.clone(), key.clone()));
                }
                if let Some(android) = &descriptor.android {
                    insert_unique(
                        &mut android_projects,
                        android.gradle_project.clone(),
                        key,
                        "Android Gradle project",
                    )?;
                    insert_unique(
                        &mut android_namespaces,
                        android.namespace.clone(),
                        key,
                        "Android namespace",
                    )?;
                    insert_unique(
                        &mut android_coordinates,
                        android.maven_coordinate.clone(),
                        key,
                        "Android Maven coordinate",
                    )?;
                    for source in &android.sources {
                        for (other, owner) in &roots {
                            if overlap(source, other) {
                                return Err(invalid(format!(
                                    "overlapping source roots in {owner} and {key}: {other} / {source}"
                                )));
                            }
                        }
                        roots.push((source.clone(), key.clone()));
                    }
                }
            }
            State::Retired => {
                if let Some(package) = &descriptor.reserved_ffi_package {
                    insert_unique(&mut packages, package.clone(), key, "reserved FFI package")?;
                }
                if let Some(library) = &descriptor.reserved_library_stem {
                    insert_unique(
                        &mut libraries,
                        library.clone(),
                        key,
                        "reserved library stem",
                    )?;
                }
                if let Some(value) = &descriptor.reserved_android_gradle_project {
                    insert_unique(
                        &mut android_projects,
                        value.clone(),
                        key,
                        "reserved Android Gradle project",
                    )?;
                }
                if let Some(value) = &descriptor.reserved_android_namespace {
                    insert_unique(
                        &mut android_namespaces,
                        value.clone(),
                        key,
                        "reserved Android namespace",
                    )?;
                }
                if let Some(value) = &descriptor.reserved_android_maven_coordinate {
                    insert_unique(
                        &mut android_coordinates,
                        value.clone(),
                        key,
                        "reserved Android Maven coordinate",
                    )?;
                }
            }
        }
    }

    for (key, record) in &records {
        let descriptor = &record.descriptor;
        if descriptor.state != State::Active {
            continue;
        }
        let owner = records.get(&descriptor.artifact_owner).ok_or_else(|| {
            invalid(format!(
                "{key} artifact_owner does not exist: {}",
                descriptor.artifact_owner
            ))
        })?;
        if owner.descriptor.state != State::Active
            || owner.descriptor.artifact_owner != owner.descriptor.key
        {
            return Err(invalid(format!(
                "{key} artifact_owner must be an active self-owning record: {}",
                descriptor.artifact_owner
            )));
        }
    }

    Ok(Catalog { records })
}

fn catalog_present(repo: &Path, reference: &str) -> Result<bool> {
    Ok(exact_entry(repo, reference, CATALOG_README)?.is_some()
        || !tree_entries(repo, reference, CATALOG_ROOT)?.is_empty())
}

fn toml_string(value: &str) -> Result<String> {
    toml::Value::String(value.to_owned())
        .to_string()
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .map(|escaped| format!("\"{escaped}\""))
        .ok_or_else(|| invalid("failed to render TOML string"))
}

fn render_tombstone(base: &Descriptor, pr: u64, url: &str) -> Result<Vec<u8>> {
    if base.state != State::Active {
        return Err(invalid("only an active component can be retired"));
    }
    if pr == 0 || url != format!("https://github.com/pablof7z/nmp/pull/{pr}") {
        return Err(invalid("retirement PR number/URL do not match"));
    }
    let mut output = String::new();
    output.push_str("schema = 1\n");
    output.push_str(&format!("key = {}\n", toml_string(&base.key)?));
    output.push_str("state = \"retired\"\n");
    output.push_str(&format!(
        "uniffi_namespace = {}\n",
        toml_string(&base.uniffi_namespace)?
    ));
    output.push_str(&format!(
        "artifact_owner = {}\n",
        toml_string(&base.artifact_owner)?
    ));
    if base.artifact_owner == base.key {
        output.push_str(&format!(
            "reserved_ffi_package = {}\n",
            toml_string(
                base.ffi_package
                    .as_deref()
                    .ok_or_else(|| invalid("active owner has no ffi_package"))?
            )?
        ));
        output.push_str(&format!(
            "reserved_library_stem = {}\n",
            toml_string(
                base.library_stem
                    .as_deref()
                    .ok_or_else(|| invalid("active owner has no library_stem"))?
            )?
        ));
    }
    if let Some(android) = &base.android {
        output.push_str(&format!(
            "reserved_android_gradle_project = {}\n",
            toml_string(&android.gradle_project)?
        ));
        output.push_str(&format!(
            "reserved_android_namespace = {}\n",
            toml_string(&android.namespace)?
        ));
        output.push_str(&format!(
            "reserved_android_maven_coordinate = {}\n",
            toml_string(&android.maven_coordinate)?
        ));
    }
    output.push_str(&format!("retired_by_pr = {pr}\n"));
    output.push_str(&format!("retired_by_url = {}\n", toml_string(url)?));
    output.push_str("removed_paths = [\n");
    for path in active_removed_paths(base)? {
        output.push_str(&format!("  {},\n", toml_string(&path)?));
    }
    output.push_str("]\n");
    Ok(output.into_bytes())
}

fn check_stable_active(base: &Descriptor, head: &Descriptor) -> Result<()> {
    for (label, base_value, head_value) in [
        ("key", &base.key, &head.key),
        (
            "uniffi_namespace",
            &base.uniffi_namespace,
            &head.uniffi_namespace,
        ),
        ("artifact_owner", &base.artifact_owner, &head.artifact_owner),
    ] {
        if base_value != head_value {
            return Err(invalid(format!(
                "active component {} changed stable {label}: {base_value} -> {head_value}",
                base.key
            )));
        }
    }
    if base.library_stem != head.library_stem {
        return Err(invalid(format!(
            "active component {} changed stable library_stem",
            base.key
        )));
    }
    if base.ffi_package != head.ffi_package {
        return Err(invalid(format!(
            "active component {} changed stable ffi_package",
            base.key
        )));
    }
    let base_android = base.android.as_ref();
    let head_android = head.android.as_ref();
    if base_android.map(|android| {
        (
            &android.gradle_project,
            &android.namespace,
            &android.maven_coordinate,
        )
    }) != head_android.map(|android| {
        (
            &android.gradle_project,
            &android.namespace,
            &android.maven_coordinate,
        )
    }) {
        return Err(invalid(format!(
            "active component {} changed stable Android package identities",
            base.key
        )));
    }
    Ok(())
}

fn transition(
    repo: &Path,
    base_ref: &str,
    head_ref: &str,
    pr: u64,
    url: &str,
) -> Result<&'static str> {
    let base_present = catalog_present(repo, base_ref)?;
    let head_present = catalog_present(repo, head_ref)?;
    match (base_present, head_present) {
        (false, false) => Err(invalid(
            "component catalog is absent from both base and head",
        )),
        (true, false) => Err(invalid("component catalog was deleted from the head")),
        (false, true) => {
            let legacy = exact_entry(repo, base_ref, LEGACY_SNAPSHOT)?;
            if legacy.as_ref().is_none_or(|entry| !regular_blob(entry)) {
                return Err(invalid(
                    "catalog bootstrap requires the regular legacy core snapshot on the base",
                ));
            }
            let head = validate_catalog(repo, head_ref)?;
            let keys = head.records.keys().cloned().collect::<BTreeSet<_>>();
            let expected = BTreeSet::from(["nmp-core".to_owned(), "nmp-nip46".to_owned()]);
            if keys != expected
                || head
                    .records
                    .values()
                    .any(|record| record.descriptor.state != State::Active)
            {
                return Err(invalid(
                    "catalog bootstrap must contain exactly active nmp-core and nmp-nip46 records",
                ));
            }
            Ok("bootstrap")
        }
        (true, true) => {
            let base = validate_catalog(repo, base_ref)?;
            let head = validate_catalog(repo, head_ref)?;
            for (key, base_record) in &base.records {
                let head_record = head.records.get(key).ok_or_else(|| {
                    invalid(format!(
                        "active/retired descriptor cannot be deleted or renamed: {key}"
                    ))
                })?;
                match (base_record.descriptor.state, head_record.descriptor.state) {
                    (State::Active, State::Active) => {
                        check_stable_active(&base_record.descriptor, &head_record.descriptor)?;
                    }
                    (State::Active, State::Retired) => {
                        let expected = render_tombstone(&base_record.descriptor, pr, url)?;
                        if head_record.bytes != expected {
                            return Err(invalid(format!(
                                "{key} retirement tombstone is not the exact derived record"
                            )));
                        }
                        let live_children = head
                            .records
                            .values()
                            .filter(|record| {
                                record.descriptor.state == State::Active
                                    && record.descriptor.artifact_owner == *key
                                    && record.descriptor.key != *key
                            })
                            .map(|record| record.descriptor.key.clone())
                            .collect::<Vec<_>>();
                        if !live_children.is_empty() {
                            return Err(invalid(format!(
                                "cannot retire artifact owner {key} with live children: {live_children:?}"
                            )));
                        }
                    }
                    (State::Retired, State::Retired) => {
                        if base_record.entry.path != head_record.entry.path
                            || base_record.entry.mode != head_record.entry.mode
                            || base_record.bytes != head_record.bytes
                        {
                            return Err(invalid(format!(
                                "retired tombstone is path/mode/byte immutable: {key}"
                            )));
                        }
                    }
                    (State::Retired, State::Active) => {
                        return Err(invalid(format!(
                            "retired component cannot reactivate: {key}"
                        )));
                    }
                }
            }
            for (key, head_record) in &head.records {
                if !base.records.contains_key(key) && head_record.descriptor.state == State::Retired
                {
                    return Err(invalid(format!(
                        "a new component cannot begin as retired: {key}"
                    )));
                }
            }
            Ok("steady")
        }
    }
}

fn changed_paths(repo: &Path, base_ref: &str, head_ref: &str) -> Result<BTreeSet<String>> {
    let range = format!("{base_ref}...{head_ref}");
    let bytes = git(
        repo,
        &[
            OsStr::new("diff"),
            OsStr::new("--name-status"),
            OsStr::new("-z"),
            OsStr::new(&range),
        ],
    )?;
    let fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| {
            std::str::from_utf8(field)
                .map(str::to_owned)
                .map_err(|_| invalid("git diff path/status was not UTF-8"))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut index = 0;
    let mut paths = BTreeSet::new();
    while index < fields.len() {
        let status = &fields[index];
        index += 1;
        let count = if status.starts_with('R') || status.starts_with('C') {
            2
        } else {
            1
        };
        if index + count > fields.len() {
            return Err(invalid("truncated git diff --name-status -z output"));
        }
        for path in &fields[index..index + count] {
            paths.insert(path.clone());
        }
        index += count;
    }
    Ok(paths)
}

fn union_active_records<'a>(
    base: Option<&'a Catalog>,
    head: Option<&'a Catalog>,
) -> Vec<&'a Descriptor> {
    let mut records = BTreeMap::new();
    for catalog in [base, head].into_iter().flatten() {
        for (key, record) in &catalog.records {
            if record.descriptor.state == State::Active {
                records.insert(key.clone(), &record.descriptor);
            }
        }
    }
    records.into_values().collect()
}

fn under(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn projections(repo: &Path, base_ref: &str, head_ref: &str) -> Result<String> {
    let paths = changed_paths(repo, base_ref, head_ref)?;
    let base = if catalog_present(repo, base_ref)? {
        Some(validate_catalog(repo, base_ref)?)
    } else {
        None
    };
    let head = if catalog_present(repo, head_ref)? {
        Some(validate_catalog(repo, head_ref)?)
    } else {
        None
    };
    let records = union_active_records(base.as_ref(), head.as_ref());
    let mut rust = false;
    let mut ffi = false;
    let mut swift = false;
    let mut kotlin = false;
    for path in paths {
        rust |= path == RUST_FACADE_SNAPSHOT;
        ffi |= path == LEGACY_SNAPSHOT
            || path
                .strip_prefix(&format!("{CATALOG_ROOT}/"))
                .is_some_and(|suffix| {
                    suffix.ends_with("/component.toml") || suffix.ends_with("/uniffi.txt")
                });
        for descriptor in &records {
            for root in descriptor
                .swift_manifests
                .as_ref()
                .into_iter()
                .flatten()
                .chain(descriptor.swift_sources.as_ref().into_iter().flatten())
            {
                swift |= under(&path, root);
            }
            for root in descriptor
                .kotlin_manifests
                .as_ref()
                .into_iter()
                .flatten()
                .chain(descriptor.kotlin_sources.as_ref().into_iter().flatten())
            {
                kotlin |= under(&path, root);
            }
            if let Some(android) = &descriptor.android {
                for root in android.manifests.iter().chain(&android.sources) {
                    kotlin |= under(&path, root);
                }
            }
        }
    }
    let mut values = Vec::new();
    if ffi {
        values.push("ffi");
    }
    if kotlin {
        values.push("kotlin");
    }
    if rust {
        values.push("rust");
    }
    if swift {
        values.push("swift");
    }
    Ok(if values.is_empty() {
        "none".to_owned()
    } else {
        values.join(",")
    })
}

fn write_nul_fields(fields: &[&str]) -> Result<()> {
    let mut stdout = io::stdout().lock();
    for field in fields {
        stdout.write_all(field.as_bytes())?;
        stdout.write_all(&[0])?;
    }
    Ok(())
}

fn active_rows(catalog: &Catalog) -> Result<()> {
    for (key, record) in &catalog.records {
        let descriptor = &record.descriptor;
        if descriptor.state != State::Active {
            continue;
        }
        let owner = catalog
            .records
            .get(&descriptor.artifact_owner)
            .ok_or_else(|| invalid("validated artifact owner disappeared"))?;
        write_nul_fields(&[
            key,
            &descriptor.artifact_owner,
            &descriptor.uniffi_namespace,
            owner
                .descriptor
                .ffi_package
                .as_deref()
                .ok_or_else(|| invalid("validated owner has no ffi_package"))?,
            owner
                .descriptor
                .ffi_manifest
                .as_deref()
                .ok_or_else(|| invalid("validated owner has no ffi_manifest"))?,
            owner
                .descriptor
                .library_stem
                .as_deref()
                .ok_or_else(|| invalid("validated owner has no library_stem"))?,
            &snapshot_path(key),
        ])?;
    }
    Ok(())
}

fn parity_rows(catalog: &Catalog) -> Result<()> {
    let mut stdout = io::stdout().lock();
    for (key, record) in &catalog.records {
        let descriptor = &record.descriptor;
        if descriptor.state != State::Active {
            continue;
        }
        let mut rows = vec![format!("{key}\tmeta\t{}", descriptor.uniffi_namespace)];
        for root in required_vec(&descriptor.ffi_sources, "ffi_sources")? {
            rows.push(format!("{key}\tffi\t{root}"));
        }
        for root in required_vec(&descriptor.swift_sources, "swift_sources")? {
            rows.push(format!("{key}\tswift\t{root}"));
        }
        if let Some(reason) = &descriptor.swift_omission_reason {
            rows.push(format!("{key}\tomit-swift\t{reason}"));
        }
        for root in required_vec(&descriptor.kotlin_sources, "kotlin_sources")? {
            rows.push(format!("{key}\tkotlin\t{root}"));
        }
        if let Some(reason) = &descriptor.kotlin_omission_reason {
            rows.push(format!("{key}\tomit-kotlin\t{reason}"));
        }
        if let Some(android) = &descriptor.android {
            for root in &android.sources {
                rows.push(format!("{key}\tkotlin\t{root}"));
            }
        }
        for row in rows {
            stdout.write_all(row.as_bytes())?;
            stdout.write_all(&[0])?;
        }
    }
    Ok(())
}

fn allowlist_rows(repo: &Path, reference: &str, catalog: &Catalog) -> Result<()> {
    let entry = exact_entry(repo, reference, ALLOWLIST)?
        .ok_or_else(|| invalid(format!("parity allowlist is missing: {ALLOWLIST}")))?;
    if !regular_blob(&entry) || entry.mode != "100644" {
        return Err(invalid("parity allowlist must be a regular 0644 file"));
    }
    let bytes = blob(repo, &entry.oid)?;
    if bytes.len() > MAX_DESCRIPTOR_BYTES {
        return Err(invalid(format!(
            "parity allowlist exceeds {MAX_DESCRIPTOR_BYTES} bytes"
        )));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid("parity allowlist is not UTF-8"))?;
    let allowlist: Allowlist =
        toml::from_str(text).map_err(|error| invalid(format!("{ALLOWLIST}: {error}")))?;
    if allowlist.schema != 1 {
        return Err(invalid(format!(
            "unsupported parity allowlist schema {}",
            allowlist.schema
        )));
    }
    let mut tuples = BTreeSet::new();
    let mut previous = None;
    let mut rows = Vec::new();
    for exception in allowlist.exception {
        let record = catalog.records.get(&exception.component).ok_or_else(|| {
            invalid(format!(
                "parity exception names unknown component: {}",
                exception.component
            ))
        })?;
        if record.descriptor.state != State::Active {
            return Err(invalid(format!(
                "parity exception names retired component: {}",
                exception.component
            )));
        }
        if exception.concept.len() < 3
            || !exception
                .concept
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            return Err(invalid(format!(
                "parity exception concept is malformed: {}",
                exception.concept
            )));
        }
        if !matches!(exception.platform.as_str(), "swift" | "kotlin") {
            return Err(invalid(format!(
                "parity exception platform is malformed: {}",
                exception.platform
            )));
        }
        nonempty(&exception.justification, "parity exception justification")?;
        let tuple = (
            exception.component.clone(),
            exception.concept.clone(),
            exception.platform.clone(),
        );
        if !tuples.insert(tuple.clone()) {
            return Err(invalid(
                "duplicate component/concept/platform parity exception",
            ));
        }
        if previous.as_ref().is_some_and(|prior| prior >= &tuple) {
            return Err(invalid(
                "parity exceptions must be in canonical component/concept/platform order",
            ));
        }
        previous = Some(tuple);
        rows.push(format!(
            "{}\t{}\t{}\t{}",
            exception.component, exception.concept, exception.platform, exception.justification
        ));
    }
    let mut stdout = io::stdout().lock();
    for row in rows {
        stdout.write_all(row.as_bytes())?;
        stdout.write_all(&[0])?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = env::args().collect::<Vec<_>>();
    let command = args.get(1).map(String::as_str).ok_or_else(usage)?;
    match command {
        "validate" if args.len() == 4 => {
            validate_catalog(Path::new(&args[2]), &args[3])?;
            println!("surface-component-catalog: {} is valid", args[3]);
        }
        "transition" if args.len() == 7 => {
            let pr = args[5]
                .parse::<u64>()
                .map_err(|_| invalid("PR number must be a positive integer"))?;
            let mode = transition(Path::new(&args[2]), &args[3], &args[4], pr, &args[6])?;
            println!("{mode}");
        }
        "projections" if args.len() == 5 => {
            println!("{}", projections(Path::new(&args[2]), &args[3], &args[4])?);
        }
        "active-rows" if args.len() == 4 => {
            let catalog = validate_catalog(Path::new(&args[2]), &args[3])?;
            active_rows(&catalog)?;
        }
        "parity-rows" if args.len() == 4 => {
            let catalog = validate_catalog(Path::new(&args[2]), &args[3])?;
            parity_rows(&catalog)?;
        }
        "allowlist-rows" if args.len() == 4 => {
            let repo = Path::new(&args[2]);
            let catalog = validate_catalog(repo, &args[3])?;
            allowlist_rows(repo, &args[3], &catalog)?;
        }
        "render-tombstone" if args.len() == 7 => {
            let repo = Path::new(&args[2]);
            let catalog = validate_catalog(repo, &args[3])?;
            let record = catalog
                .records
                .get(&args[4])
                .ok_or_else(|| invalid(format!("unknown component: {}", args[4])))?;
            let pr = args[5]
                .parse::<u64>()
                .map_err(|_| invalid("PR number must be a positive integer"))?;
            io::stdout().write_all(&render_tombstone(&record.descriptor, pr, &args[6])?)?;
        }
        _ => return Err(usage()),
    }
    Ok(())
}
