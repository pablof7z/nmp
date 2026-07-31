use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use nmp_component_artifact_witness::{canonical_json, digest_file, plan_localization, witness};

fn main() {
    if let Err(error) = run() {
        eprintln!("component-artifact-witness: refused: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut arguments = env::args_os();
    let executable = arguments.next().unwrap_or_default();
    let Some(command) = arguments.next() else {
        return usage(&executable);
    };
    let options = Options::parse(arguments.collect())?;
    match command.to_str() {
        Some("witness") => {
            let artifact = options.required_path("--artifact")?;
            let target = options.required_string("--target")?;
            let component_key = options.required_string("--component-key")?;
            let attestation_symbol = options.required_string("--attestation-symbol")?;
            let forbidden = options.optional_path("--forbid-symbols")?;
            options.finish()?;
            let result = witness(
                &artifact,
                &target,
                &component_key,
                &attestation_symbol,
                forbidden.as_deref(),
            )?;
            print!("{}", String::from_utf8(canonical_json(&result)?)?);
        }
        Some("plan-localization") => {
            let artifact = options.required_path("--artifact")?;
            let target = options.required_string("--target")?;
            let namespace = options.required_string("--interface-namespace")?;
            let output = options.required_path("--out")?;
            options.finish()?;
            let (result, exact_symbols) = plan_localization(&artifact, &target, &namespace)?;
            atomic_write(&output, &exact_symbols)?;
            print!("{}", String::from_utf8(canonical_json(&result)?)?);
        }
        Some("digest") => {
            let path = options.required_path("--file")?;
            options.finish()?;
            print!("{}", digest_file(&path)?);
        }
        _ => return usage(&executable),
    }
    Ok(())
}

fn usage(executable: &OsString) -> Result<()> {
    bail!(
        "usage:\n  {} witness --artifact PATH --target TRIPLE --component-key KEY \
         --attestation-symbol SYMBOL [--forbid-symbols NUL_FILE]\n  \
         {} plan-localization --artifact PATH --target TRIPLE \
         --interface-namespace NAMESPACE --out NUL_FILE\n  \
         {} digest --file PATH",
        PathBuf::from(executable).display(),
        PathBuf::from(executable).display(),
        PathBuf::from(executable).display()
    )
}

#[derive(Debug)]
struct Options {
    values: std::cell::RefCell<Vec<(String, OsString)>>,
}

impl Options {
    fn parse(arguments: Vec<OsString>) -> Result<Self> {
        let (pairs, remainder) = arguments.as_chunks::<2>();
        if !remainder.is_empty() {
            bail!("every option requires exactly one value");
        }
        let mut values = Vec::new();
        for [raw_name, value] in pairs {
            let name = raw_name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("option name is not valid UTF-8"))?;
            if !name.starts_with("--") {
                bail!("unexpected positional argument {name:?}");
            }
            if values.iter().any(|(existing, _)| existing == name) {
                bail!("duplicate option {name}");
            }
            values.push((name.to_owned(), value.clone()));
        }
        Ok(Self {
            values: std::cell::RefCell::new(values),
        })
    }

    fn take(&self, name: &str) -> Option<OsString> {
        let mut values = self.values.borrow_mut();
        values
            .iter()
            .position(|(found, _)| found == name)
            .map(|index| values.remove(index).1)
    }

    fn required_path(&self, name: &str) -> Result<PathBuf> {
        self.take(name)
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("missing required option {name}"))
    }

    fn optional_path(&self, name: &str) -> Result<Option<PathBuf>> {
        Ok(self.take(name).map(PathBuf::from))
    }

    fn required_string(&self, name: &str) -> Result<String> {
        self.take(name)
            .ok_or_else(|| anyhow::anyhow!("missing required option {name}"))?
            .into_string()
            .map_err(|_| anyhow::anyhow!("{name} value is not valid UTF-8"))
    }

    fn finish(&self) -> Result<()> {
        let remaining = self.values.borrow();
        if !remaining.is_empty() {
            bail!(
                "unknown option(s): {}",
                remaining
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("output path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create output directory {}", parent.display()))?;
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes)
        .with_context(|| format!("write temporary output {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("publish exact-symbol output {}", path.display()))?;
    Ok(())
}
