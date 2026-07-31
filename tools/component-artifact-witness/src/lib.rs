//! Fail-closed inspection of final NMP native-component artifacts.
//!
//! The artifact bytes are the authority. File extensions, `nm`, `strings`, and
//! caller-authored identity fields are deliberately not used as evidence.

#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Seek, SeekFrom, Write},
    path::Path,
};

use anyhow::{anyhow, bail, ensure, Context, Result};
use goblin::{
    archive::Archive,
    elf::{
        header::{ET_DYN, ET_REL},
        section_header::SHN_UNDEF,
        sym::{STB_GLOBAL, STB_WEAK, STV_DEFAULT, STV_PROTECTED},
        Elf,
    },
    mach::{
        constants::cputype::{CPU_TYPE_ARM64, CPU_TYPE_X86_64},
        header::{MH_DYLIB, MH_OBJECT},
        Mach, MachO,
    },
    Object,
};
use serde::Serialize;
use serde_json::Value;
use uniffi_bindgen::{library_mode, EmptyCrateConfigSupplier};

const ARCHIVE_MAGIC: &[u8; 8] = b"!<arch>\n";
const THIN_ARCHIVE_MAGIC: &[u8; 8] = b"!<thin>\n";
const BITCODE_MAGIC: &[u8; 4] = b"BC\xc0\xde";
const BITCODE_WRAPPER_MAGIC: &[u8; 4] = b"\xde\xc0\x17\x0b";
const ATTESTATION_MAGIC: &[u8; 8] = b"NMPATT01";
const MAX_ATTESTATION_BYTES: usize = 64 * 1024;
const CORE_IDENTITY_PREFIX: &str = "nmp-core-component-v2-";
const OPTIONAL_IDENTITY_PREFIX: &str = "nmp-nip46-component-v2-";
const INTERFACE_IDENTITY_PREFIX: &str = "nmp-component-interface-v2-";

#[derive(Clone, Copy, Debug)]
struct ComponentAuthority {
    component_key: &'static str,
    kind: &'static str,
    cargo_package: &'static str,
    library_stem: &'static str,
    uniffi_namespace: &'static str,
    attestation_symbol: &'static str,
    identity_prefix: &'static str,
}

const CORE_AUTHORITY: ComponentAuthority = ComponentAuthority {
    component_key: "nmp-core",
    kind: "core",
    cargo_package: "nmp-ffi",
    library_stem: "nmp_ffi",
    uniffi_namespace: "nmp_ffi",
    attestation_symbol: "NMP_CORE_COMPONENT_ATTESTATION_V2",
    identity_prefix: CORE_IDENTITY_PREFIX,
};
const OPTIONAL_AUTHORITY: ComponentAuthority = ComponentAuthority {
    component_key: "nmp-nip46",
    kind: "optional",
    cargo_package: "nmp-nip46-ffi",
    library_stem: "nmp_nip46_ffi",
    uniffi_namespace: "nmp_nip46_ffi",
    attestation_symbol: "NMP_NIP46_COMPONENT_ATTESTATION_V2",
    identity_prefix: OPTIONAL_IDENTITY_PREFIX,
};

#[derive(Clone, Copy, Debug)]
struct InterfaceAuthority {
    uniffi_namespace: &'static str,
    identity_prefix: &'static str,
}

const INTERFACE_AUTHORITY: InterfaceAuthority = InterfaceAuthority {
    uniffi_namespace: "nmp_component_interface",
    identity_prefix: INTERFACE_IDENTITY_PREFIX,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObjectFamily {
    Elf,
    Mach,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TargetSpec {
    family: ObjectFamily,
    architecture: Architecture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Architecture {
    Aarch64,
    X86_64,
}

impl Architecture {
    fn label(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64",
            Self::X86_64 => "x86_64",
        }
    }
}

impl TargetSpec {
    fn parse(target: &str) -> Result<Self> {
        let architecture = if target.starts_with("aarch64-") {
            Architecture::Aarch64
        } else if target.starts_with("x86_64-") {
            Architecture::X86_64
        } else {
            bail!("unsupported component target architecture: {target}");
        };
        let family = if target.contains("-apple-") {
            ObjectFamily::Mach
        } else if target.contains("-linux-") {
            ObjectFamily::Elf
        } else {
            bail!("unsupported component target object family: {target}");
        };
        Ok(Self {
            family,
            architecture,
        })
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SymbolOccurrence {
    raw_name: String,
    normalized_name: String,
    owner: String,
    data: Option<Vec<u8>>,
}

#[derive(Debug)]
struct ArtifactAnalysis {
    format: String,
    architecture: Architecture,
    public_symbols: Vec<SymbolOccurrence>,
}

#[derive(Debug, Serialize)]
pub struct UniFfiComponentWitness {
    namespace: String,
    callables: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ArtifactWitness {
    architecture: String,
    artifact_blake3: String,
    artifact_size: u64,
    attestation: Value,
    component_key: String,
    format: String,
    public_symbols: Vec<String>,
    schema: u32,
    target: String,
    uniffi_components: Vec<UniFfiComponentWitness>,
}

#[derive(Debug, Serialize)]
pub struct LocalizationPlanWitness {
    artifact_blake3: String,
    interface_namespace: String,
    schema: u32,
    symbols: Vec<String>,
}

pub fn witness(
    artifact: &Path,
    target: &str,
    component_key: &str,
    attestation_symbol: &str,
    forbidden_symbols: Option<&Path>,
) -> Result<ArtifactWitness> {
    validate_component_key(component_key)?;
    validate_symbol_argument(attestation_symbol, "attestation symbol")?;
    let authority = component_authority(component_key)?;
    ensure_attestation_symbol(authority, attestation_symbol)?;
    let target_spec = TargetSpec::parse(target)?;
    let bytes = fs::read(artifact)
        .with_context(|| format!("read component artifact {}", artifact.display()))?;
    let analysis = analyze_bytes(&bytes, target_spec)?;
    let attestation = select_attestation(&analysis.public_symbols, attestation_symbol)?;
    validate_attestation(&attestation, authority, target)?;
    let components = extract_uniffi_components(&bytes)?;
    validate_component_namespaces(authority, &components)?;

    if let Some(path) = forbidden_symbols {
        let forbidden = read_nul_symbol_file(path)?;
        let public = analysis
            .public_symbols
            .iter()
            .map(|symbol| symbol.raw_name.as_str())
            .collect::<BTreeSet<_>>();
        let leaked = forbidden
            .iter()
            .filter(|symbol| public.contains(symbol.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        ensure!(
            leaked.is_empty(),
            "artifact still publicly defines forbidden exact symbols: {leaked:?}"
        );
    }

    let public_symbols = analysis
        .public_symbols
        .iter()
        .map(|symbol| symbol.raw_name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(ArtifactWitness {
        architecture: analysis.architecture.label().to_owned(),
        artifact_blake3: blake3::hash(&bytes).to_hex().to_string(),
        artifact_size: bytes.len() as u64,
        attestation,
        component_key: component_key.to_owned(),
        format: analysis.format,
        public_symbols,
        schema: 1,
        target: target.to_owned(),
        uniffi_components: components,
    })
}

pub fn plan_localization(
    artifact: &Path,
    target: &str,
    interface_namespace: &str,
) -> Result<(LocalizationPlanWitness, Vec<u8>)> {
    validate_namespace(interface_namespace)?;
    plan_namespace_authority(interface_namespace)?;
    let target_spec = TargetSpec::parse(target)?;
    let bytes = fs::read(artifact)
        .with_context(|| format!("read localization source {}", artifact.display()))?;
    let analysis = analyze_bytes(&bytes, target_spec)?;
    ensure!(
        analysis.format.starts_with("archive-"),
        "localization plans require an ordinary static archive, found {}",
        analysis.format
    );
    let components = extract_uniffi_components(&bytes)?;
    let planned = localization_symbols(&analysis, &components, interface_namespace)?;

    let mut nul = Vec::new();
    for symbol in &planned {
        ensure!(
            !symbol.as_bytes().contains(&0),
            "symbol name contains an embedded NUL"
        );
        nul.extend_from_slice(symbol.as_bytes());
        nul.push(0);
    }
    let symbols = planned.into_iter().collect::<Vec<_>>();
    Ok((
        LocalizationPlanWitness {
            artifact_blake3: blake3::hash(&bytes).to_hex().to_string(),
            interface_namespace: interface_namespace.to_owned(),
            schema: 1,
            symbols,
        },
        nul,
    ))
}

fn plan_namespace_authority(interface_namespace: &str) -> Result<()> {
    ensure!(
        interface_namespace == INTERFACE_AUTHORITY.uniffi_namespace,
        "localization interface namespace must be exactly {:?}",
        INTERFACE_AUTHORITY.uniffi_namespace
    );
    Ok(())
}

fn localization_symbols(
    analysis: &ArtifactAnalysis,
    components: &[UniFfiComponentWitness],
    interface_namespace: &str,
) -> Result<BTreeSet<String>> {
    let matching = components
        .iter()
        .filter(|component| component.namespace == interface_namespace)
        .collect::<Vec<_>>();
    ensure!(
        matching.len() == 1,
        "expected exactly one compiled UniFFI namespace {interface_namespace:?}, found {}",
        matching.len()
    );
    let component = matching[0];
    let owner_prefix = format!("{interface_namespace}-");
    let owned = analysis
        .public_symbols
        .iter()
        .filter(|symbol| {
            Path::new(&symbol.owner)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&owner_prefix))
        })
        .collect::<Vec<_>>();
    ensure!(
        !owned.is_empty(),
        "archive has no structurally attributable {interface_namespace} object members"
    );

    let mut planned = BTreeSet::new();
    for callable in &component.callables {
        let occurrences = owned
            .iter()
            .filter(|symbol| symbol.normalized_name == *callable)
            .collect::<Vec<_>>();
        ensure!(
            occurrences.len() == 1,
            "compiled callable {interface_namespace}::{callable} has {} public definitions in interface-owned members",
            occurrences.len()
        );
        planned.insert(occurrences[0].raw_name.clone());
    }

    let metadata = owned
        .iter()
        .filter(|symbol| symbol.normalized_name.starts_with("UNIFFI_META_"))
        .collect::<Vec<_>>();
    ensure!(
        !metadata.is_empty(),
        "interface-owned members contain no compiled UniFFI metadata symbols"
    );
    let namespace_marker = format!(
        "UNIFFI_META_NAMESPACE_{}",
        interface_namespace.to_ascii_uppercase()
    );
    ensure!(
        metadata
            .iter()
            .any(|symbol| symbol.normalized_name == namespace_marker),
        "interface-owned members contain no exact namespace marker {namespace_marker}"
    );
    planned.extend(metadata.into_iter().map(|symbol| symbol.raw_name.clone()));
    ensure!(
        !planned.is_empty(),
        "localization plan unexpectedly contains no symbols"
    );
    Ok(planned)
}

pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let value = serde_json::to_value(value).context("convert witness to canonical JSON value")?;
    let mut bytes = serde_json::to_vec(&value).context("serialize canonical witness JSON")?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn digest_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read digest input {}", path.display()))?;
    Ok(format!("{}\n", blake3::hash(&bytes).to_hex()))
}

fn analyze_bytes(bytes: &[u8], target: TargetSpec) -> Result<ArtifactAnalysis> {
    reject_magic(bytes)?;
    match Object::parse(bytes).context("parse artifact by magic")? {
        Object::Archive(archive) => analyze_archive(bytes, &archive, target),
        Object::Elf(elf) => {
            ensure!(
                target.family == ObjectFamily::Elf,
                "ELF artifact disagrees with target"
            );
            analyze_elf(bytes, &elf, target, "<dynamic>", false)
        }
        Object::Mach(Mach::Binary(mach)) => {
            ensure!(
                target.family == ObjectFamily::Mach,
                "Mach-O artifact disagrees with target"
            );
            analyze_mach(bytes, &mach, target, "<dynamic>", false)
        }
        Object::Mach(Mach::Fat(_)) => {
            bail!("fat/universal Mach-O is unsupported; witness each thin target slice")
        }
        Object::PE(_) | Object::COFF(_) => bail!("PE/COFF artifacts are unsupported"),
        Object::Unknown(magic) => bail!("unsupported artifact magic 0x{magic:016x}"),
        _ => bail!("unsupported native artifact container"),
    }
}

fn analyze_archive(
    bytes: &[u8],
    archive: &Archive<'_>,
    target: TargetSpec,
) -> Result<ArtifactAnalysis> {
    ensure!(
        bytes.starts_with(ARCHIVE_MAGIC),
        "only ordinary SysV/BSD archives are supported"
    );
    ensure!(archive.len() > 0, "archive contains no object members");
    let mut public_symbols = Vec::new();
    for index in 0..archive.len() {
        let member = archive
            .get_at(index)
            .ok_or_else(|| anyhow!("archive member index {index} disappeared"))?;
        let start = usize::try_from(member.offset).context("archive member offset overflow")?;
        let end = start
            .checked_add(member.size())
            .ok_or_else(|| anyhow!("archive member size overflow"))?;
        let member_bytes = bytes
            .get(start..end)
            .ok_or_else(|| anyhow!("archive member {} is out of bounds", member.extended_name()))?;
        reject_magic(member_bytes)
            .with_context(|| format!("unsupported archive member {}", member.extended_name()))?;
        let owner = member.extended_name();
        match Object::parse(member_bytes)
            .with_context(|| format!("parse archive member {owner}"))?
        {
            Object::Elf(elf) => {
                ensure!(
                    target.family == ObjectFamily::Elf,
                    "ELF archive member {owner} disagrees with target"
                );
                let analyzed = analyze_elf(member_bytes, &elf, target, owner, true)?;
                public_symbols.extend(analyzed.public_symbols);
            }
            Object::Mach(Mach::Binary(mach)) => {
                ensure!(
                    target.family == ObjectFamily::Mach,
                    "Mach-O archive member {owner} disagrees with target"
                );
                let analyzed = analyze_mach(member_bytes, &mach, target, owner, true)?;
                public_symbols.extend(analyzed.public_symbols);
            }
            Object::Mach(Mach::Fat(_)) => {
                bail!("archive member {owner} is fat/universal Mach-O")
            }
            Object::PE(_) | Object::COFF(_) => {
                bail!("archive member {owner} is unsupported PE/COFF")
            }
            Object::Archive(_) => bail!("nested archive member {owner} is unsupported"),
            Object::Unknown(magic) => {
                bail!("archive member {owner} has unsupported magic 0x{magic:016x}")
            }
            _ => bail!("archive member {owner} has an unsupported object format"),
        }
    }
    ensure!(
        !public_symbols.is_empty(),
        "archive contains no public native symbols"
    );
    Ok(ArtifactAnalysis {
        format: match target.family {
            ObjectFamily::Elf => "archive-elf".to_owned(),
            ObjectFamily::Mach => "archive-macho".to_owned(),
        },
        architecture: target.architecture,
        public_symbols,
    })
}

fn analyze_elf(
    bytes: &[u8],
    elf: &Elf<'_>,
    target: TargetSpec,
    owner: &str,
    relocatable: bool,
) -> Result<ArtifactAnalysis> {
    ensure!(elf.little_endian, "big-endian ELF is unsupported");
    let expected_machine = match target.architecture {
        Architecture::Aarch64 => goblin::elf::header::EM_AARCH64,
        Architecture::X86_64 => goblin::elf::header::EM_X86_64,
    };
    ensure!(
        elf.header.e_machine == expected_machine,
        "ELF architecture {} disagrees with target {}",
        elf.header.e_machine,
        target.architecture.label()
    );
    let expected_type = if relocatable { ET_REL } else { ET_DYN };
    ensure!(
        elf.header.e_type == expected_type,
        "ELF object type {} is not the required {}",
        elf.header.e_type,
        if relocatable { "ET_REL" } else { "ET_DYN" }
    );
    let (symbols, strings) = if relocatable {
        (&elf.syms, &elf.strtab)
    } else {
        (&elf.dynsyms, &elf.dynstrtab)
    };
    if !relocatable {
        ensure!(
            !symbols.is_empty(),
            "ELF shared object has no authoritative dynamic symbol table"
        );
    }
    let mut public_symbols = Vec::new();
    for symbol in symbols.iter() {
        let binding = symbol.st_bind();
        let visibility = symbol.st_visibility();
        if (binding != STB_GLOBAL && binding != STB_WEAK)
            || symbol.st_shndx == SHN_UNDEF as usize
            || (visibility != STV_DEFAULT && visibility != STV_PROTECTED)
        {
            continue;
        }
        let Some(name) = strings.get_at(symbol.st_name) else {
            bail!("ELF public symbol has no valid UTF-8 name");
        };
        if name.is_empty() {
            continue;
        }
        let data = elf_symbol_data(bytes, elf, &symbol)
            .with_context(|| format!("read ELF symbol data for {name}"))?;
        public_symbols.push(SymbolOccurrence {
            raw_name: name.to_owned(),
            normalized_name: name.to_owned(),
            owner: owner.to_owned(),
            data,
        });
    }
    Ok(ArtifactAnalysis {
        format: if relocatable {
            "elf-relocatable".to_owned()
        } else {
            "elf-shared-object".to_owned()
        },
        architecture: target.architecture,
        public_symbols,
    })
}

fn elf_symbol_data(
    bytes: &[u8],
    elf: &Elf<'_>,
    symbol: &goblin::elf::sym::Sym,
) -> Result<Option<Vec<u8>>> {
    if symbol.st_size == 0 {
        return Ok(None);
    }
    let section = elf
        .section_headers
        .get(symbol.st_shndx)
        .ok_or_else(|| anyhow!("symbol section index {} is out of bounds", symbol.st_shndx))?;
    ensure!(
        symbol.st_value >= section.sh_addr,
        "symbol address precedes its section"
    );
    let within = symbol.st_value - section.sh_addr;
    let start = section
        .sh_offset
        .checked_add(within)
        .ok_or_else(|| anyhow!("ELF symbol file offset overflow"))?;
    let end = start
        .checked_add(symbol.st_size)
        .ok_or_else(|| anyhow!("ELF symbol size overflow"))?;
    let start = usize::try_from(start).context("ELF symbol start does not fit usize")?;
    let end = usize::try_from(end).context("ELF symbol end does not fit usize")?;
    let data = bytes
        .get(start..end)
        .ok_or_else(|| anyhow!("ELF symbol bytes are out of bounds"))?;
    Ok(Some(data.to_vec()))
}

fn analyze_mach(
    bytes: &[u8],
    mach: &MachO<'_>,
    target: TargetSpec,
    owner: &str,
    relocatable: bool,
) -> Result<ArtifactAnalysis> {
    ensure!(mach.little_endian, "big-endian Mach-O is unsupported");
    ensure!(mach.is_64, "32-bit Mach-O is unsupported");
    let expected_cpu = match target.architecture {
        Architecture::Aarch64 => CPU_TYPE_ARM64,
        Architecture::X86_64 => CPU_TYPE_X86_64,
    };
    ensure!(
        mach.header.cputype == expected_cpu,
        "Mach-O CPU type {} disagrees with target {}",
        mach.header.cputype,
        target.architecture.label()
    );
    let expected_filetype = if relocatable { MH_OBJECT } else { MH_DYLIB };
    ensure!(
        mach.header.filetype == expected_filetype,
        "Mach-O file type {} is not the required {}",
        mach.header.filetype,
        if relocatable { "MH_OBJECT" } else { "MH_DYLIB" }
    );

    let sections = mach_sections(mach)?;
    let mut symbol_data = BTreeMap::<String, Vec<Vec<u8>>>::new();
    let mut nlist_public = BTreeSet::new();
    for symbol in mach.symbols() {
        let (name, nlist) = symbol.context("read Mach-O symbol")?;
        if !nlist.is_global() || nlist.is_undefined() || nlist.is_stab() || name.is_empty() {
            continue;
        }
        nlist_public.insert(name.to_owned());
        if let Some(data) = mach_symbol_data(bytes, &sections, &nlist)? {
            symbol_data.entry(name.to_owned()).or_default().push(data);
        }
    }

    let public_names = if relocatable {
        nlist_public
    } else {
        let exports = mach.exports().context("read Mach-O export trie")?;
        ensure!(
            !exports.is_empty(),
            "Mach-O dylib has no authoritative export trie"
        );
        exports.into_iter().map(|export| export.name).collect()
    };
    let mut public_symbols = Vec::new();
    for raw_name in public_names {
        let data = match symbol_data.remove(&raw_name) {
            None => None,
            Some(mut values) if values.len() == 1 => values.pop(),
            Some(values) => {
                bail!(
                    "Mach-O public symbol {raw_name} has {} defined data locations",
                    values.len()
                )
            }
        };
        public_symbols.push(SymbolOccurrence {
            normalized_name: normalize_mach_symbol(&raw_name).to_owned(),
            raw_name,
            owner: owner.to_owned(),
            data,
        });
    }
    Ok(ArtifactAnalysis {
        format: if relocatable {
            "macho-relocatable".to_owned()
        } else {
            "macho-dylib".to_owned()
        },
        architecture: target.architecture,
        public_symbols,
    })
}

fn mach_sections(mach: &MachO<'_>) -> Result<Vec<goblin::mach::segment::Section>> {
    let mut sections = Vec::new();
    for segment in &mach.segments {
        for section in segment {
            let (section, _) = section.context("read Mach-O section")?;
            sections.push(section);
        }
    }
    Ok(sections)
}

fn mach_symbol_data(
    bytes: &[u8],
    sections: &[goblin::mach::segment::Section],
    symbol: &goblin::mach::symbols::Nlist,
) -> Result<Option<Vec<u8>>> {
    if symbol.n_sect == 0 {
        return Ok(None);
    }
    let section = sections
        .get(symbol.n_sect - 1)
        .ok_or_else(|| anyhow!("Mach-O symbol section {} is out of bounds", symbol.n_sect))?;
    ensure!(
        symbol.n_value >= section.addr,
        "Mach-O symbol address precedes its section"
    );
    let within = symbol.n_value - section.addr;
    ensure!(
        within < section.size,
        "Mach-O symbol lies outside its section"
    );
    let start = u64::from(section.offset)
        .checked_add(within)
        .ok_or_else(|| anyhow!("Mach-O symbol file offset overflow"))?;
    let remaining = section.size - within;
    let end = start
        .checked_add(remaining)
        .ok_or_else(|| anyhow!("Mach-O section size overflow"))?;
    let start = usize::try_from(start).context("Mach-O symbol start does not fit usize")?;
    let end = usize::try_from(end).context("Mach-O symbol end does not fit usize")?;
    Ok(Some(
        bytes
            .get(start..end)
            .ok_or_else(|| anyhow!("Mach-O symbol bytes are out of bounds"))?
            .to_vec(),
    ))
}

fn normalize_mach_symbol(name: &str) -> &str {
    name.strip_prefix('_').unwrap_or(name)
}

fn select_attestation(symbols: &[SymbolOccurrence], requested: &str) -> Result<Value> {
    let matches = symbols
        .iter()
        .filter(|symbol| symbol.normalized_name == requested)
        .collect::<Vec<_>>();
    ensure!(
        matches.len() == 1,
        "expected exactly one public attestation symbol {requested:?}, found {}",
        matches.len()
    );
    let data = matches[0]
        .data
        .as_deref()
        .ok_or_else(|| anyhow!("attestation symbol {requested:?} has no readable data"))?;
    parse_attestation_bytes(data)
}

fn parse_attestation_bytes(data: &[u8]) -> Result<Value> {
    ensure!(
        data.len() >= ATTESTATION_MAGIC.len() + 4,
        "attestation record is truncated"
    );
    ensure!(
        &data[..ATTESTATION_MAGIC.len()] == ATTESTATION_MAGIC,
        "attestation magic disagrees"
    );
    let length = u32::from_le_bytes(
        data[ATTESTATION_MAGIC.len()..ATTESTATION_MAGIC.len() + 4]
            .try_into()
            .expect("four-byte attestation length"),
    ) as usize;
    ensure!(
        length <= MAX_ATTESTATION_BYTES,
        "attestation payload exceeds {MAX_ATTESTATION_BYTES} bytes"
    );
    let start = ATTESTATION_MAGIC.len() + 4;
    let end = start
        .checked_add(length)
        .ok_or_else(|| anyhow!("attestation length overflow"))?;
    let payload = data
        .get(start..end)
        .ok_or_else(|| anyhow!("attestation payload is truncated"))?;
    let value: Value = serde_json::from_slice(payload).context("parse attestation JSON")?;
    ensure!(value.is_object(), "attestation JSON must be an object");
    let canonical = serde_json::to_vec(&value).context("canonicalize attestation JSON")?;
    ensure!(
        canonical == payload,
        "attestation JSON is not canonical sorted compact JSON"
    );
    Ok(value)
}

fn component_authority(component_key: &str) -> Result<&'static ComponentAuthority> {
    match component_key {
        "nmp-core" => Ok(&CORE_AUTHORITY),
        "nmp-nip46" => Ok(&OPTIONAL_AUTHORITY),
        other => bail!("unknown component authority {other:?}"),
    }
}

fn ensure_attestation_symbol(
    authority: &ComponentAuthority,
    attestation_symbol: &str,
) -> Result<()> {
    ensure!(
        attestation_symbol == authority.attestation_symbol,
        "attestation symbol for {:?} must be exactly {:?}",
        authority.component_key,
        authority.attestation_symbol
    );
    Ok(())
}

fn validate_component_namespaces(
    authority: &ComponentAuthority,
    components: &[UniFfiComponentWitness],
) -> Result<()> {
    let observed = components
        .iter()
        .map(|component| component.namespace.as_str())
        .collect::<BTreeSet<_>>();
    ensure!(
        observed.contains(authority.uniffi_namespace),
        "artifact for {:?} lacks its authoritative UniFFI namespace {:?}",
        authority.component_key,
        authority.uniffi_namespace
    );
    let allowed = BTreeSet::from([
        authority.uniffi_namespace,
        INTERFACE_AUTHORITY.uniffi_namespace,
    ]);
    let unexpected = observed.difference(&allowed).copied().collect::<Vec<_>>();
    ensure!(
        unexpected.is_empty(),
        "artifact for {:?} contains non-authoritative UniFFI namespaces: {unexpected:?}",
        authority.component_key
    );
    Ok(())
}

fn validate_attestation(value: &Value, authority: &ComponentAuthority, target: &str) -> Result<()> {
    let fields = value
        .as_object()
        .ok_or_else(|| anyhow!("attestation must be an object"))?;
    let kind = json_string(fields, "kind")?;
    ensure!(
        kind == authority.kind,
        "attestation kind for {:?} must be exactly {:?}",
        authority.component_key,
        authority.kind
    );
    let mut expected = BTreeSet::from([
        "build_flags_digest",
        "cargo_package",
        "component_key",
        "graph_digest",
        "identity",
        "interface_identity",
        "kind",
        "library_stem",
        "profile",
        "rustc_digest",
        "schema",
        "target",
        "uniffi_namespace",
    ]);
    match authority.kind {
        "core" => {}
        "optional" => {
            expected.extend([
                "required_core_artifact_blake3",
                "required_core_identity",
                "required_core_manifest_blake3",
            ]);
        }
        impossible => bail!("compiled component authority has invalid kind {impossible:?}"),
    }
    let actual = fields.keys().map(String::as_str).collect::<BTreeSet<_>>();
    ensure!(
        actual == expected,
        "attestation exact fields disagree; missing={:?}, unknown={:?}",
        expected.difference(&actual).collect::<Vec<_>>(),
        actual.difference(&expected).collect::<Vec<_>>()
    );
    ensure!(
        fields.get("schema").and_then(Value::as_u64) == Some(1),
        "attestation schema must be exactly 1"
    );
    ensure!(
        json_string(fields, "component_key")? == authority.component_key,
        "attestation component_key must be exactly {:?}",
        authority.component_key
    );
    validate_component_key(json_string(fields, "component_key")?)?;
    ensure_authority_field(fields, "cargo_package", authority.cargo_package)?;
    ensure_authority_field(fields, "library_stem", authority.library_stem)?;
    ensure_authority_field(fields, "uniffi_namespace", authority.uniffi_namespace)?;
    ensure!(
        json_string(fields, "profile")? == "release",
        "attestation profile must be exactly \"release\""
    );
    let attested_target = json_string(fields, "target")?;
    ensure_target_token(attested_target)?;
    TargetSpec::parse(attested_target).context("attestation target is unsupported")?;
    ensure!(
        attested_target == target,
        "attestation target does not match requested target"
    );
    ensure_identity_prefix(
        json_string(fields, "identity")?,
        "identity",
        authority.identity_prefix,
    )?;
    ensure_identity_prefix(
        json_string(fields, "interface_identity")?,
        "interface_identity",
        INTERFACE_AUTHORITY.identity_prefix,
    )?;
    for field in ["build_flags_digest", "graph_digest", "rustc_digest"] {
        ensure_digest(json_string(fields, field)?, field)?;
    }
    if kind == "optional" {
        ensure_identity_prefix(
            json_string(fields, "required_core_identity")?,
            "required_core_identity",
            CORE_IDENTITY_PREFIX,
        )?;
        ensure_digest(
            json_string(fields, "required_core_artifact_blake3")?,
            "required_core_artifact_blake3",
        )?;
        ensure_digest(
            json_string(fields, "required_core_manifest_blake3")?,
            "required_core_manifest_blake3",
        )?;
    }
    Ok(())
}

fn json_string<'a>(fields: &'a serde_json::Map<String, Value>, name: &str) -> Result<&'a str> {
    fields
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("attestation field {name} must be a non-empty string"))
}

fn ensure_authority_field(
    fields: &serde_json::Map<String, Value>,
    name: &str,
    expected: &str,
) -> Result<()> {
    ensure!(
        json_string(fields, name)? == expected,
        "attestation {name} must be exactly {expected:?}"
    );
    Ok(())
}

fn ensure_identity_prefix(value: &str, field: &str, prefix: &str) -> Result<()> {
    let digest = value.strip_prefix(prefix);
    ensure!(
        digest.is_some_and(is_hex_digest),
        "attestation {field} must be exactly {prefix}<64-lowercase-hex>"
    );
    Ok(())
}

fn ensure_digest(value: &str, field: &str) -> Result<()> {
    ensure!(
        is_hex_digest(value),
        "attestation {field} is not a 64-hex BLAKE3 digest"
    );
    Ok(())
}

fn ensure_target_token(value: &str) -> Result<()> {
    ensure!(
        value.split('-').all(|segment| {
            !segment.is_empty()
                && segment.as_bytes()[0].is_ascii_alphanumeric()
                && segment.as_bytes()[segment.len() - 1].is_ascii_alphanumeric()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
                && !segment.contains("__")
        }),
        "attestation target is not a stable Rust target token"
    );
    Ok(())
}

fn is_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn extract_uniffi_components(bytes: &[u8]) -> Result<Vec<UniFfiComponentWitness>> {
    with_immutable_snapshot(bytes, |path| {
        let path = path
            .to_str()
            .ok_or_else(|| anyhow!("snapshot path is not valid UTF-8: {}", path.display()))?;
        let components = library_mode::find_components(path.into(), &EmptyCrateConfigSupplier)
            .context("extract compiled UniFFI components from immutable snapshot")?;
        let witnesses = components
            .into_iter()
            .map(|component| UniFfiComponentWitness {
                namespace: component.ci.namespace().to_owned(),
                callables: component
                    .ci
                    .iter_ffi_function_definitions()
                    .map(|function| function.name().to_owned())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            })
            .collect();
        finish_uniffi_components(witnesses)
    })
}

fn with_immutable_snapshot<T>(bytes: &[u8], inspect: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    #[cfg(not(unix))]
    {
        let _ = (bytes, inspect);
        bail!("artifact snapshots require a Linux or Apple file-descriptor host");
    }
    #[cfg(unix)]
    let mut snapshot = tempfile::tempfile().context("create anonymous artifact snapshot")?;
    #[cfg(unix)]
    snapshot
        .write_all(bytes)
        .context("write anonymous artifact snapshot")?;
    #[cfg(unix)]
    snapshot
        .flush()
        .context("flush anonymous artifact snapshot")?;
    #[cfg(unix)]
    snapshot
        .seek(SeekFrom::Start(0))
        .context("rewind anonymous artifact snapshot")?;
    #[cfg(unix)]
    let mut permissions = snapshot
        .metadata()
        .context("stat anonymous artifact snapshot")?
        .permissions();
    #[cfg(unix)]
    permissions.set_readonly(true);
    #[cfg(unix)]
    snapshot
        .set_permissions(permissions)
        .context("seal anonymous artifact snapshot read-only")?;
    #[cfg(unix)]
    let descriptor_path = if cfg!(target_os = "linux") {
        format!("/proc/self/fd/{}", snapshot.as_raw_fd())
    } else if cfg!(target_vendor = "apple") {
        format!("/dev/fd/{}", snapshot.as_raw_fd())
    } else {
        bail!("artifact snapshots require a Linux or Apple file-descriptor host");
    };
    #[cfg(unix)]
    {
        inspect(Path::new(&descriptor_path))
    }
}

fn finish_uniffi_components(
    mut witnesses: Vec<UniFfiComponentWitness>,
) -> Result<Vec<UniFfiComponentWitness>> {
    witnesses.sort_by(|left, right| left.namespace.cmp(&right.namespace));
    if let Some(duplicate) = witnesses
        .windows(2)
        .find(|pair| pair[0].namespace == pair[1].namespace)
    {
        bail!(
            "compiled UniFFI namespace {:?} appears more than once",
            duplicate[0].namespace
        );
    }
    Ok(witnesses)
}

fn read_nul_symbol_file(path: &Path) -> Result<BTreeSet<String>> {
    let bytes =
        fs::read(path).with_context(|| format!("read exact-symbol file {}", path.display()))?;
    ensure!(
        !bytes.is_empty() && bytes.last() == Some(&0),
        "exact-symbol file must be non-empty and NUL-terminated"
    );
    let mut symbols = BTreeSet::new();
    for field in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
        ensure!(
            !field.is_empty(),
            "exact-symbol file contains an empty field"
        );
        let symbol = std::str::from_utf8(field)
            .context("exact-symbol file contains a non-UTF-8 field")?
            .to_owned();
        ensure!(
            symbols.insert(symbol.clone()),
            "exact-symbol file contains duplicate symbol {symbol:?}"
        );
    }
    Ok(symbols)
}

fn validate_component_key(value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "component key is empty");
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && value.as_bytes()[0].is_ascii_lowercase()
            && !value.ends_with('-')
            && !value.contains("--"),
        "component key is not stable kebab-case: {value:?}"
    );
    Ok(())
}

fn validate_namespace(value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "UniFFI namespace is empty");
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            && value.as_bytes()[0].is_ascii_lowercase(),
        "invalid UniFFI namespace {value:?}"
    );
    Ok(())
}

fn validate_symbol_argument(value: &str, label: &str) -> Result<()> {
    ensure!(!value.is_empty(), "{label} is empty");
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "{label} contains unsupported characters: {value:?}"
    );
    Ok(())
}

fn reject_magic(bytes: &[u8]) -> Result<()> {
    if bytes.starts_with(THIN_ARCHIVE_MAGIC) {
        bail!("thin archives are unsupported");
    }
    if bytes.starts_with(BITCODE_MAGIC) || bytes.starts_with(BITCODE_WRAPPER_MAGIC) {
        bail!("LLVM bitcode is unsupported");
    }
    if bytes.starts_with(b"MZ") {
        bail!("PE/COFF artifacts are unsupported");
    }
    if bytes.len() >= 4 {
        let magic = &bytes[..4];
        if matches!(
            magic,
            [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
                | [0xca, 0xfe, 0xba, 0xbf]
                | [0xbf, 0xba, 0xfe, 0xca]
        ) {
            bail!("fat/universal Mach-O is unsupported; witness each thin target slice");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };

    const CORE_IDENTITY: &str =
        "nmp-core-component-v2-1111111111111111111111111111111111111111111111111111111111111111";
    const INTERFACE_IDENTITY: &str =
        "nmp-component-interface-v2-2222222222222222222222222222222222222222222222222222222222222222";

    #[test]
    fn thin_archive_is_rejected() {
        let error = analyze_bytes(
            THIN_ARCHIVE_MAGIC,
            TargetSpec::parse("x86_64-unknown-linux-gnu").unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("thin archives"));
    }

    #[test]
    fn raw_bitcode_is_rejected() {
        for magic in [BITCODE_MAGIC.as_slice(), BITCODE_WRAPPER_MAGIC.as_slice()] {
            let error = analyze_bytes(
                magic,
                TargetSpec::parse("x86_64-unknown-linux-gnu").unwrap(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("LLVM bitcode"));
        }
    }

    #[test]
    fn pe_is_rejected_before_extension_or_parser_fallback() {
        let error = analyze_bytes(
            b"MZfake",
            TargetSpec::parse("x86_64-unknown-linux-gnu").unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("PE/COFF"));
    }

    #[test]
    fn coff_object_is_rejected() {
        let mut coff = Vec::new();
        coff.extend_from_slice(&goblin::pe::header::COFF_MACHINE_X86_64.to_le_bytes());
        coff.extend_from_slice(&0u16.to_le_bytes());
        coff.extend_from_slice(&0u32.to_le_bytes());
        coff.extend_from_slice(&0u32.to_le_bytes());
        coff.extend_from_slice(&0u32.to_le_bytes());
        coff.extend_from_slice(&0u16.to_le_bytes());
        coff.extend_from_slice(&0u16.to_le_bytes());
        let error = analyze_bytes(
            &coff,
            TargetSpec::parse("x86_64-unknown-linux-gnu").unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("PE/COFF"));
    }

    #[test]
    fn fat_mach_is_rejected() {
        for magic in [
            [0xca, 0xfe, 0xba, 0xbe],
            [0xbe, 0xba, 0xfe, 0xca],
            [0xca, 0xfe, 0xba, 0xbf],
            [0xbf, 0xba, 0xfe, 0xca],
        ] {
            let bytes = [magic.as_slice(), &[0, 0, 0, 0]].concat();
            let error = analyze_bytes(&bytes, TargetSpec::parse("x86_64-apple-darwin").unwrap())
                .unwrap_err();
            assert!(error.to_string().contains("fat/universal"));
        }
    }

    #[test]
    fn unsupported_target_and_container_are_rejected() {
        let error = TargetSpec::parse("wasm32-unknown-unknown").unwrap_err();
        assert!(error.to_string().contains("unsupported"));
        let error = analyze_bytes(
            &[0; 64],
            TargetSpec::parse("x86_64-unknown-linux-gnu").unwrap(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("unsupported artifact"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn archive_bitcode_member_is_rejected() {
        let archive = one_member_archive("fixture.o", BITCODE_MAGIC);
        let error = analyze_bytes(
            &archive,
            TargetSpec::parse("x86_64-unknown-linux-gnu").unwrap(),
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("fixture.o"));
        assert!(text.contains("LLVM bitcode"));
    }

    #[test]
    fn wrong_mach_architecture_is_rejected() {
        let bytes = minimal_mach_header(CPU_TYPE_X86_64, MH_DYLIB);
        let error =
            analyze_bytes(&bytes, TargetSpec::parse("aarch64-apple-darwin").unwrap()).unwrap_err();
        assert!(error.to_string().contains("disagrees with target"));
    }

    #[test]
    fn wrong_elf_architecture_is_rejected() {
        let bytes = minimal_elf_header(goblin::elf::header::EM_X86_64);
        let error = analyze_bytes(
            &bytes,
            TargetSpec::parse("aarch64-unknown-linux-gnu").unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("disagrees with target"));
    }

    #[test]
    fn missing_attestation_is_rejected() {
        let error = select_attestation(&[], "NMP_MISSING_ATTESTATION").unwrap_err();
        assert!(error.to_string().contains("found 0"));
    }

    #[test]
    fn ambiguous_attestation_is_rejected() {
        let payload = attestation_record(core_attestation("aarch64-apple-darwin"));
        let symbols = vec![
            SymbolOccurrence {
                raw_name: "NMP_DUPLICATE".to_owned(),
                normalized_name: "NMP_DUPLICATE".to_owned(),
                owner: "one.o".to_owned(),
                data: Some(payload.clone()),
            },
            SymbolOccurrence {
                raw_name: "NMP_DUPLICATE".to_owned(),
                normalized_name: "NMP_DUPLICATE".to_owned(),
                owner: "two.o".to_owned(),
                data: Some(payload),
            },
        ];
        let error = select_attestation(&symbols, "NMP_DUPLICATE").unwrap_err();
        assert!(error.to_string().contains("found 2"));
    }

    #[test]
    fn ambiguous_uniffi_namespace_is_rejected() {
        let duplicate = || UniFfiComponentWitness {
            namespace: "nmp_duplicate".to_owned(),
            callables: vec!["ffi_nmp_duplicate_contract_version".to_owned()],
        };
        let error = finish_uniffi_components(vec![duplicate(), duplicate()]).unwrap_err();
        assert!(error.to_string().contains("appears more than once"));
    }

    #[test]
    fn original_path_swap_cannot_change_extracted_components_or_plan() {
        let temp = unique_temp_dir();
        fs::create_dir_all(&temp).unwrap();
        let original = temp.join("component.a");
        fs::write(&original, b"component-a").unwrap();
        let captured = fs::read(&original).unwrap();

        let components = with_immutable_snapshot(&captured, |snapshot| {
            fs::write(&original, b"component-b").unwrap();
            assert!(fs::metadata(snapshot)?.permissions().readonly());
            let namespace = match fs::read(snapshot)?.as_slice() {
                b"component-a" => "nmp_a",
                b"component-b" => "nmp_b",
                other => bail!("unexpected snapshot bytes: {other:?}"),
            };
            finish_uniffi_components(vec![UniFfiComponentWitness {
                namespace: namespace.to_owned(),
                callables: vec![format!("ffi_{namespace}_call")],
            }])
        })
        .unwrap();
        assert_eq!(fs::read(&original).unwrap(), b"component-b");
        assert_eq!(components[0].namespace, "nmp_a");

        let analysis = ArtifactAnalysis {
            format: "archive-macho".to_owned(),
            architecture: Architecture::Aarch64,
            public_symbols: vec![
                SymbolOccurrence {
                    raw_name: "_ffi_nmp_a_call".to_owned(),
                    normalized_name: "ffi_nmp_a_call".to_owned(),
                    owner: "nmp_a-fixture.rcgu.o".to_owned(),
                    data: None,
                },
                SymbolOccurrence {
                    raw_name: "_UNIFFI_META_NAMESPACE_NMP_A".to_owned(),
                    normalized_name: "UNIFFI_META_NAMESPACE_NMP_A".to_owned(),
                    owner: "nmp_a-fixture.rcgu.o".to_owned(),
                    data: None,
                },
            ],
        };
        let planned = localization_symbols(&analysis, &components, "nmp_a").unwrap();
        assert_eq!(
            planned,
            BTreeSet::from([
                "_UNIFFI_META_NAMESPACE_NMP_A".to_owned(),
                "_ffi_nmp_a_call".to_owned(),
            ])
        );
        fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn exact_snapshot_descriptor_path_cannot_be_replaced() {
        let temp = unique_temp_dir();
        fs::create_dir_all(&temp).unwrap();
        let replacement = temp.join("replacement");
        fs::write(&replacement, b"component-b").unwrap();

        with_immutable_snapshot(b"component-a", |snapshot| {
            assert!(
                snapshot.starts_with("/proc/self/fd") || snapshot.starts_with("/dev/fd"),
                "snapshot is not descriptor-backed: {}",
                snapshot.display()
            );
            assert!(fs::rename(&replacement, snapshot).is_err());
            assert!(fs::remove_file(snapshot).is_err());
            assert_eq!(fs::read(snapshot)?, b"component-a");
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&replacement).unwrap(), b"component-b");
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn digest_is_exact_lowercase_hex_with_one_newline() {
        let temp = unique_temp_dir();
        fs::create_dir_all(&temp).unwrap();
        let input = temp.join("input.bin");
        fs::write(&input, b"abc").unwrap();
        assert_eq!(
            digest_file(&input).unwrap(),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85\n"
        );
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn digest_changes_after_one_byte_flip() {
        let temp = unique_temp_dir();
        fs::create_dir_all(&temp).unwrap();
        let input = temp.join("input.bin");
        fs::write(&input, b"abc").unwrap();
        let before = digest_file(&input).unwrap();
        fs::write(&input, b"abb").unwrap();
        let after = digest_file(&input).unwrap();
        assert_ne!(after, before);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn digest_refuses_a_missing_file() {
        let missing = unique_temp_dir().join("missing.bin");
        let error = digest_file(&missing).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("read digest input"));
        assert!(text.contains("missing.bin"));
    }

    #[test]
    fn attestation_requires_canonical_exact_shape() {
        let mut noncanonical = Vec::from(ATTESTATION_MAGIC.as_slice());
        let payload = format!(
            "{{ \"schema\": 1, \"kind\": \"core\", \"component_key\": \"nmp-core\", \"identity\": \"{CORE_IDENTITY}\", \"interface_identity\": \"{INTERFACE_IDENTITY}\" }}"
        );
        noncanonical.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        noncanonical.extend_from_slice(payload.as_bytes());
        let error = parse_attestation_bytes(&noncanonical).unwrap_err();
        assert!(error.to_string().contains("not canonical"));

        let target = "aarch64-apple-darwin";
        let valid = parse_attestation_bytes(&attestation_record(core_attestation(target))).unwrap();
        validate_attestation(&valid, &CORE_AUTHORITY, target).unwrap();

        let linux_target = "x86_64-unknown-linux-gnu";
        validate_attestation(
            &core_attestation(linux_target),
            &CORE_AUTHORITY,
            linux_target,
        )
        .unwrap();
    }

    #[test]
    fn attestation_requires_every_exact_field_and_refuses_unknown_fields() {
        let target = "aarch64-apple-darwin";
        let mut missing = core_attestation(target);
        missing
            .as_object_mut()
            .unwrap()
            .remove("build_flags_digest");
        let error = validate_attestation(&missing, &CORE_AUTHORITY, target).unwrap_err();
        assert!(error.to_string().contains("build_flags_digest"));

        let mut unknown = core_attestation(target);
        unknown
            .as_object_mut()
            .unwrap()
            .insert("invented".to_owned(), Value::Bool(true));
        let error = validate_attestation(&unknown, &CORE_AUTHORITY, target).unwrap_err();
        assert!(error.to_string().contains("invented"));

        validate_attestation(&optional_attestation(target), &OPTIONAL_AUTHORITY, target).unwrap();
    }

    #[test]
    fn attestation_refuses_malformed_bound_fields() {
        let target = "aarch64-apple-darwin";
        for (field, malformed) in [
            ("build_flags_digest", "ABC"),
            ("cargo_package", "NMP FFI"),
            ("graph_digest", "44"),
            ("library_stem", "nmp-ffi"),
            ("profile", "debug"),
            ("rustc_digest", "55"),
            ("target", "wasm32-unknown-unknown"),
            ("uniffi_namespace", "NMP_FFI"),
        ] {
            let mut value = core_attestation(target);
            value
                .as_object_mut()
                .unwrap()
                .insert(field.to_owned(), Value::String(malformed.to_owned()));
            let error = validate_attestation(&value, &CORE_AUTHORITY, target).unwrap_err();
            assert!(
                format!("{error:#}").contains(field),
                "{field} produced unexpected error: {error:#}"
            );
        }
    }

    #[test]
    fn compiled_authority_refuses_kind_confusion_and_unknown_callers() {
        let target = "aarch64-apple-darwin";
        let mut provider_as_core = optional_attestation(target);
        let provider_fields = provider_as_core.as_object_mut().unwrap();
        provider_fields.insert("kind".to_owned(), Value::String("core".to_owned()));
        provider_fields.remove("required_core_artifact_blake3");
        provider_fields.remove("required_core_identity");
        provider_fields.remove("required_core_manifest_blake3");
        let error =
            validate_attestation(&provider_as_core, &OPTIONAL_AUTHORITY, target).unwrap_err();
        assert!(error.to_string().contains("kind"));

        let mut core_as_optional = core_attestation(target);
        let core_fields = core_as_optional.as_object_mut().unwrap();
        core_fields.insert("kind".to_owned(), Value::String("optional".to_owned()));
        core_fields.insert(
            "required_core_artifact_blake3".to_owned(),
            Value::String("7".repeat(64)),
        );
        core_fields.insert(
            "required_core_identity".to_owned(),
            Value::String(CORE_IDENTITY.to_owned()),
        );
        core_fields.insert(
            "required_core_manifest_blake3".to_owned(),
            Value::String("8".repeat(64)),
        );
        let error = validate_attestation(&core_as_optional, &CORE_AUTHORITY, target).unwrap_err();
        assert!(error.to_string().contains("kind"));

        let provider_components = vec![
            UniFfiComponentWitness {
                namespace: OPTIONAL_AUTHORITY.uniffi_namespace.to_owned(),
                callables: Vec::new(),
            },
            UniFfiComponentWitness {
                namespace: INTERFACE_AUTHORITY.uniffi_namespace.to_owned(),
                callables: Vec::new(),
            },
        ];
        validate_component_namespaces(&OPTIONAL_AUTHORITY, &provider_components).unwrap();
        let error =
            validate_component_namespaces(&CORE_AUTHORITY, &provider_components).unwrap_err();
        assert!(error.to_string().contains("authoritative UniFFI namespace"));

        let error = component_authority("nmp-unknown").unwrap_err();
        assert!(error.to_string().contains("unknown component authority"));
        for symbol in [
            "NMP_CORE_COMPONENT_ATTESTATION_V1",
            "NMP_NIP46_COMPONENT_ATTESTATION_V1",
            "NMP_UNKNOWN_COMPONENT_ATTESTATION_V2",
        ] {
            let authority = if symbol.contains("NIP46") {
                &OPTIONAL_AUTHORITY
            } else {
                &CORE_AUTHORITY
            };
            let error = ensure_attestation_symbol(authority, symbol).unwrap_err();
            assert!(error.to_string().contains("must be exactly"));
        }
        let error = plan_namespace_authority("unknown_interface").unwrap_err();
        assert!(error.to_string().contains("must be exactly"));
    }

    #[test]
    fn v1_and_unknown_identity_prefixes_are_permanently_rejected() {
        let target = "aarch64-apple-darwin";
        let cases = [
            (
                &CORE_AUTHORITY,
                core_attestation(target),
                "identity",
                format!("nmp-core-component-v1-{}", "1".repeat(64)),
            ),
            (
                &OPTIONAL_AUTHORITY,
                optional_attestation(target),
                "identity",
                format!("nmp-nip46-component-v1-{}", "6".repeat(64)),
            ),
            (
                &CORE_AUTHORITY,
                core_attestation(target),
                "interface_identity",
                format!("nmp-component-interface-v1-{}", "2".repeat(64)),
            ),
            (
                &OPTIONAL_AUTHORITY,
                optional_attestation(target),
                "required_core_identity",
                format!("nmp-core-component-v1-{}", "1".repeat(64)),
            ),
            (
                &CORE_AUTHORITY,
                core_attestation(target),
                "identity",
                format!("unknown-component-v2-{}", "1".repeat(64)),
            ),
        ];
        for (authority, mut value, field, malformed) in cases {
            value
                .as_object_mut()
                .unwrap()
                .insert(field.to_owned(), Value::String(malformed));
            let error = validate_attestation(&value, authority, target).unwrap_err();
            assert!(
                error.to_string().contains(field),
                "{field} produced unexpected error: {error:#}"
            );
        }
    }

    #[test]
    fn host_cdylib_has_structural_symbols_and_named_attestation() {
        let temp = unique_temp_dir();
        fs::create_dir_all(&temp).unwrap();
        let source = temp.join("fixture.rs");
        let target = host_target();
        let payload = serde_json::to_vec(&core_attestation(&target)).unwrap();
        let mut record = Vec::from(ATTESTATION_MAGIC.as_slice());
        record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        record.extend_from_slice(&payload);
        let bytes = record
            .iter()
            .map(|byte| byte.to_string())
            .collect::<Vec<_>>()
            .join(",");
        fs::write(
            &source,
            format!(
                "#[used]\n#[no_mangle]\npub static NMP_TEST_ATTESTATION: [u8; {}] = [{}];\n\
                 #[no_mangle]\npub extern \"C\" fn ffi_test_callable() -> u32 {{ 7 }}\n",
                record.len(),
                bytes
            ),
        )
        .unwrap();
        let library = if cfg!(target_os = "macos") {
            temp.join("libfixture.dylib")
        } else {
            temp.join("libfixture.so")
        };
        let status = Command::new("rustc")
            .args(["--edition=2021", "--crate-type=cdylib"])
            .arg(&source)
            .arg("-o")
            .arg(&library)
            .status()
            .unwrap();
        assert!(status.success());
        let spec = TargetSpec::parse(&target).unwrap();
        let artifact = fs::read(&library).unwrap();
        let analysis = analyze_bytes(&artifact, spec).unwrap();
        assert!(analysis
            .public_symbols
            .iter()
            .any(|symbol| symbol.normalized_name == "ffi_test_callable"));
        let attestation =
            select_attestation(&analysis.public_symbols, "NMP_TEST_ATTESTATION").unwrap();
        validate_attestation(&attestation, &CORE_AUTHORITY, &target).unwrap();
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn host_staticlib_is_parsed_as_an_ordinary_native_archive() {
        let temp = unique_temp_dir();
        fs::create_dir_all(&temp).unwrap();
        let source = temp.join("fixture.rs");
        fs::write(
            &source,
            "#[no_mangle]\npub extern \"C\" fn ffi_test_callable() -> u32 { 7 }\n",
        )
        .unwrap();
        let library = temp.join("libfixture.a");
        let status = Command::new("rustc")
            .args(["--edition=2021", "--crate-type=staticlib"])
            .arg(&source)
            .arg("-o")
            .arg(&library)
            .status()
            .unwrap();
        assert!(status.success());
        let target = host_target();
        let spec = TargetSpec::parse(&target).unwrap();
        let artifact = fs::read(&library).unwrap();
        let analysis = analyze_bytes(&artifact, spec).unwrap();
        assert!(analysis.format.starts_with("archive-"));
        assert!(analysis
            .public_symbols
            .iter()
            .any(|symbol| symbol.normalized_name == "ffi_test_callable"));
        fs::remove_dir_all(temp).unwrap();
    }

    fn core_attestation(target: &str) -> Value {
        serde_json::json!({
            "build_flags_digest": "3333333333333333333333333333333333333333333333333333333333333333",
            "cargo_package": "nmp-ffi",
            "component_key": "nmp-core",
            "graph_digest": "4444444444444444444444444444444444444444444444444444444444444444",
            "identity": CORE_IDENTITY,
            "interface_identity": INTERFACE_IDENTITY,
            "kind": "core",
            "library_stem": "nmp_ffi",
            "profile": "release",
            "rustc_digest": "5555555555555555555555555555555555555555555555555555555555555555",
            "schema": 1,
            "target": target,
            "uniffi_namespace": "nmp_ffi",
        })
    }

    fn optional_attestation(target: &str) -> Value {
        serde_json::json!({
            "build_flags_digest": "3333333333333333333333333333333333333333333333333333333333333333",
            "cargo_package": "nmp-nip46-ffi",
            "component_key": "nmp-nip46",
            "graph_digest": "4444444444444444444444444444444444444444444444444444444444444444",
            "identity": "nmp-nip46-component-v2-6666666666666666666666666666666666666666666666666666666666666666",
            "interface_identity": INTERFACE_IDENTITY,
            "kind": "optional",
            "library_stem": "nmp_nip46_ffi",
            "profile": "release",
            "required_core_artifact_blake3": "7777777777777777777777777777777777777777777777777777777777777777",
            "required_core_identity": CORE_IDENTITY,
            "required_core_manifest_blake3": "8888888888888888888888888888888888888888888888888888888888888888",
            "rustc_digest": "5555555555555555555555555555555555555555555555555555555555555555",
            "schema": 1,
            "target": target,
            "uniffi_namespace": "nmp_nip46_ffi",
        })
    }

    fn attestation_record(value: Value) -> Vec<u8> {
        let payload = serde_json::to_vec(&value).unwrap();
        let mut bytes = Vec::from(ATTESTATION_MAGIC.as_slice());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes
    }

    fn one_member_archive(name: &str, contents: &[u8]) -> Vec<u8> {
        assert!(name.len() <= 15);
        let mut bytes = Vec::from(ARCHIVE_MAGIC.as_slice());
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            format!("{name}/"),
            "0",
            "0",
            "0",
            "100644",
            contents.len()
        );
        assert_eq!(header.len(), 60);
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(contents);
        if contents.len() % 2 == 1 {
            bytes.push(b'\n');
        }
        bytes
    }

    fn minimal_mach_header(cputype: u32, filetype: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        for word in [0xfeedfacfu32, cputype, 3, filetype, 0, 0, 0, 0] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    fn minimal_elf_header(machine: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; 64];
        bytes[..16].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        bytes[16..18].copy_from_slice(&ET_DYN.to_le_bytes());
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
        bytes[58..60].copy_from_slice(&64u16.to_le_bytes());
        bytes
    }

    fn host_target() -> String {
        let output = Command::new("rustc").arg("-vV").output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .unwrap()
            .to_owned()
    }

    fn unique_temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "nmp-component-artifact-witness-{}-{nonce}",
            std::process::id()
        ))
    }
}
