use std::path::PathBuf;

use nmp_behavior_traceability::{validate_repository, IssueSnapshot, TraceError};

fn main() {
    if let Err(error) = run() {
        eprintln!("behavior traceability failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), TraceError> {
    let mut arguments = std::env::args().skip(1);
    if arguments.next().as_deref() != Some("check") {
        return Err(usage());
    }
    let mut root = None;
    let mut base = None;
    let mut head = None;
    let mut issues = None;
    while let Some(argument) = arguments.next() {
        let value = arguments.next().ok_or_else(usage)?;
        match argument.as_str() {
            "--root" => root = Some(PathBuf::from(value)),
            "--base" => base = Some(value),
            "--head" => head = Some(value),
            "--issues" => issues = Some(PathBuf::from(value)),
            _ => return Err(usage()),
        }
    }
    let root = root.ok_or_else(usage)?;
    let base = base.ok_or_else(usage)?;
    let head = head.ok_or_else(usage)?;
    let issues = issues.ok_or_else(usage)?;
    reject_github_token(
        std::env::var_os("GH_TOKEN").is_some(),
        std::env::var_os("GITHUB_TOKEN").is_some(),
    )?;
    let issues = IssueSnapshot::from_path(&issues)?;
    validate_repository(&root, &base, &head, &issues)
}

fn reject_github_token(gh_token: bool, github_token: bool) -> Result<(), TraceError> {
    if gh_token || github_token {
        return Err(TraceError(
            "head-built behavior checker refuses GitHub token exposure".into(),
        ));
    }
    Ok(())
}

fn usage() -> TraceError {
    TraceError(
        "usage: nmp-behavior-traceability check --root <repo> --base <revision> --head <revision> --issues <trusted-snapshot>".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_checker_rejects_either_github_token_name() {
        assert!(reject_github_token(false, false).is_ok());
        assert!(reject_github_token(true, false).is_err());
        assert!(reject_github_token(false, true).is_err());
    }
}
