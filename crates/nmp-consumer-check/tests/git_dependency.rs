use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("consumer-check lives two levels below the repository root")
        .to_owned()
}

fn tracked_manifests(root: &Path) -> Vec<PathBuf> {
    let output = Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap(),
            "ls-files",
            "-z",
            "--",
            "*Cargo.toml",
        ])
        .output()
        .expect("git must enumerate repository manifests");
    assert!(output.status.success(), "git ls-files failed");
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| root.join(String::from_utf8(path.to_vec()).unwrap()))
        .collect()
}

fn package_name(manifest: &str) -> Option<&str> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "name" {
            return value.trim().strip_prefix('"')?.strip_suffix('"');
        }
    }
    None
}

fn temp_consumer() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("nmp-git-consumer-{}-{nonce}", std::process::id()));
    fs::create_dir_all(path.join("src")).unwrap();
    path
}

#[test]
fn repository_has_one_git_discoverable_nmp_package_and_external_consumer_builds() {
    let root = repository_root();
    let packages: Vec<PathBuf> = tracked_manifests(&root)
        .into_iter()
        .filter(|manifest| package_name(&fs::read_to_string(manifest).unwrap()) == Some("nmp"))
        .collect();
    assert_eq!(
        packages,
        vec![root.join("crates/nmp/Cargo.toml")],
        "a Git source must expose exactly the supported facade as package `nmp`"
    );

    let head = Command::new("git")
        .args(["-C", root.to_str().unwrap(), "rev-parse", "HEAD"])
        .output()
        .expect("git must resolve the tested revision");
    assert!(head.status.success());
    let head = String::from_utf8(head.stdout).unwrap();
    let head = head.trim();
    let consumer = temp_consumer();
    let root_url = format!("file://{}", root.display());
    fs::write(
        consumer.join("Cargo.toml"),
        format!(
            "[package]\nname = \"external-nmp-git-consumer\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\nnmp = {{ git = \"{root_url}\", rev = \"{head}\" }}\n"
        ),
    )
    .unwrap();
    fs::write(
        consumer.join("src/main.rs"),
        "use nmp::Engine;\nfn main() { let _engine: Option<Engine> = None; }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO"))
        .args(["check", "--quiet"])
        .current_dir(&consumer)
        .env("CARGO_TARGET_DIR", consumer.join("target"))
        .output()
        .expect("external Cargo consumer must run");
    assert!(
        output.status.success(),
        "external Git consumer failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let lock = fs::read_to_string(consumer.join("Cargo.lock")).unwrap();
    assert!(lock.contains("name = \"nmp\"\nversion = \"0.1.0\""));
    assert!(!lock.contains("name = \"nested-package\""));
    assert!(!lock.contains("name = \"renamed-package\""));

    fs::remove_dir_all(consumer).unwrap();
}
