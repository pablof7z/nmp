use clap::{Args, Parser, Subcommand, ValueEnum};
use nmp_cli::{
    verify_prepared_product, AppManifest, Catalog, PrepareOptions, Preparer, ProcessRunner,
    Product, DEFAULT_MANIFEST, DEFAULT_OUTPUT,
};
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "nmp",
    version,
    about = "Prepare capability-selected NMP products for native applications",
    long_about = "NMP application tooling. A committed .nmp.toml selects compile-time capabilities and stable product inputs. Runtime relays, accounts, signers, storage paths, and product policy remain application inputs."
)]
struct Cli {
    /// NMP source checkout. Defaults to NMP_SOURCE or the checkout that built this CLI.
    #[arg(long, global = true, env = "NMP_SOURCE")]
    source: Option<PathBuf>,
    /// Application manifest.
    #[arg(long, global = true, default_value = DEFAULT_MANIFEST)]
    manifest: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a committed .nmp.toml for an application.
    Init(InitArgs),
    /// List or edit compile-time application capabilities.
    Capability(CapabilityArgs),
    /// Build or reuse the exact local Apple/Android product selected by .nmp.toml.
    Prepare(PrepareArgs),
    /// Refuse a prepared product whose native library, wrappers, or inventory drifted.
    Verify(VerifyArgs),
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ProductArg {
    Apple,
    Android,
    KotlinJvm,
}

impl From<ProductArg> for Product {
    fn from(value: ProductArg) -> Self {
        match value {
            ProductArg::Apple => Product::Apple,
            ProductArg::Android => Product::Android,
            ProductArg::KotlinJvm => Product::KotlinJvm,
        }
    }
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Product to prepare; may be repeated.
    #[arg(long = "product", value_enum, required = true)]
    products: Vec<ProductArg>,
    /// Capability key or product-language name; may be repeated.
    #[arg(long = "capability")]
    capabilities: Vec<String>,
}

#[derive(Debug, Args)]
struct CapabilityArgs {
    #[command(subcommand)]
    command: CapabilityCommand,
}

#[derive(Debug, Subcommand)]
enum CapabilityCommand {
    /// List all application-facing capabilities and mark selected ones.
    List,
    /// Select one or more capabilities.
    Add { capabilities: Vec<String> },
    /// Remove one or more capabilities.
    Remove { capabilities: Vec<String> },
}

#[derive(Debug, Args)]
struct PrepareArgs {
    /// Generated product directory.
    #[arg(long, default_value = DEFAULT_OUTPUT)]
    output: PathBuf,
    /// Cache directory. Defaults to the platform user cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Suppress child-command progress.
    #[arg(long)]
    quiet: bool,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    /// Prepared product directory.
    #[arg(long, default_value = DEFAULT_OUTPUT)]
    output: PathBuf,
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("nmp: error: {error}");
        std::process::exit(2);
    }
}

fn run(cli: Cli) -> nmp_cli::Result<()> {
    if let Command::Verify(args) = &cli.command {
        verify_prepared_product(&args.output)?;
        println!("verified {}", args.output.display());
        return Ok(());
    }
    let root = source_root(cli.source)?;
    let catalog = Catalog::load(&root.join("native/features.toml"), &root)?;
    match cli.command {
        Command::Init(args) => {
            if cli.manifest.exists() {
                return Err(nmp_cli::Error::Refusal(format!(
                    "refusing to replace existing {}; edit it or remove it explicitly",
                    cli.manifest.display()
                )));
            }
            let mut manifest = AppManifest::new(
                cli.manifest,
                args.products.into_iter().map(Product::from).collect(),
                Vec::new(),
            )?;
            manifest.add_capabilities(&catalog, &args.capabilities)?;
            manifest.save()?;
            println!("created {}", manifest.path.display());
        }
        Command::Capability(args) => match args.command {
            CapabilityCommand::List => {
                let selected = if cli.manifest.is_file() {
                    AppManifest::load(&cli.manifest)?.capabilities
                } else {
                    Vec::new()
                };
                for feature in &catalog.features {
                    let mark = if selected.contains(&feature.key) {
                        "*"
                    } else {
                        "-"
                    };
                    println!("{mark} {:<12} {}", feature.key, feature.capability);
                }
            }
            CapabilityCommand::Add { capabilities } => {
                if capabilities.is_empty() {
                    return Err(nmp_cli::Error::Refusal(
                        "name at least one capability to add".into(),
                    ));
                }
                let mut manifest = AppManifest::load(&cli.manifest)?;
                manifest.add_capabilities(&catalog, &capabilities)?;
                manifest.save()?;
                println!("updated {}", manifest.path.display());
            }
            CapabilityCommand::Remove { capabilities } => {
                if capabilities.is_empty() {
                    return Err(nmp_cli::Error::Refusal(
                        "name at least one capability to remove".into(),
                    ));
                }
                let mut manifest = AppManifest::load(&cli.manifest)?;
                manifest.remove_capabilities(&catalog, &capabilities)?;
                manifest.save()?;
                println!("updated {}", manifest.path.display());
            }
        },
        Command::Prepare(args) => {
            let manifest = AppManifest::load(&cli.manifest)?;
            let runner = ProcessRunner {
                verbose: !args.quiet,
            };
            let result = Preparer::new(
                root,
                catalog,
                &runner,
                PrepareOptions {
                    output: args.output,
                    cache_dir: args.cache_dir.unwrap_or_else(default_cache_dir),
                },
            )
            .prepare(&manifest)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&result).map_err(|error| {
                    nmp_cli::Error::Refusal(format!("cannot render preparation result: {error}"))
                })?
            );
        }
        Command::Verify(_) => unreachable!("verify returns before source discovery"),
    }
    Ok(())
}

fn source_root(configured: Option<PathBuf>) -> nmp_cli::Result<PathBuf> {
    let root = configured.unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("nmp-cli is <repo>/crates/nmp-cli")
            .to_path_buf()
    });
    let root = if root.is_absolute() {
        root
    } else {
        std::env::current_dir()
            .map_err(|error| {
                nmp_cli::Error::Refusal(format!("cannot resolve NMP source: {error}"))
            })?
            .join(root)
    };
    if !root.join("native/features.toml").is_file() || !root.join("Cargo.lock").is_file() {
        return Err(nmp_cli::Error::Refusal(format!(
            "{} is not an NMP source checkout; pass --source or set NMP_SOURCE",
            root.display()
        )));
    }
    Ok(root)
}

fn default_cache_dir() -> PathBuf {
    if let Some(value) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(value).join("nmp-native");
    }
    if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Library/Caches/nmp-native")
    } else {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".cache/nmp-native")
    }
}
