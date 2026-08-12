use crate::{Error, Result};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

#[derive(Clone, Debug, Default)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
}

pub trait CommandRunner: Send + Sync {
    fn run(
        &self,
        args: &[String],
        cwd: &Path,
        env: &BTreeMap<String, String>,
        capture: bool,
    ) -> Result<CommandOutput>;
}

#[derive(Clone, Debug)]
pub struct ProcessRunner {
    pub verbose: bool,
}

impl CommandRunner for ProcessRunner {
    fn run(
        &self,
        args: &[String],
        cwd: &Path,
        env: &BTreeMap<String, String>,
        capture: bool,
    ) -> Result<CommandOutput> {
        let Some((program, tail)) = args.split_first() else {
            return Err(crate::refusal("internal error: empty command"));
        };
        if self.verbose {
            eprintln!("+ {}", args.join(" "));
        }
        let mut command = Command::new(program);
        command.args(tail).current_dir(cwd).envs(env);
        if capture {
            let output = command.output().map_err(|source| Error::CommandStart {
                command: args.join(" "),
                source,
            })?;
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            if !output.status.success() {
                let text = if stderr.trim().is_empty() {
                    &stdout
                } else {
                    &stderr
                };
                return Err(Error::CommandFailed {
                    command: args.join(" "),
                    status: output
                        .status
                        .code()
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "signal".into()),
                    detail: if text.trim().is_empty() {
                        String::new()
                    } else {
                        format!("\n{}", text.trim())
                    },
                });
            }
            Ok(CommandOutput { stdout, stderr })
        } else {
            let status = command.status().map_err(|source| Error::CommandStart {
                command: args.join(" "),
                source,
            })?;
            if !status.success() {
                return Err(Error::CommandFailed {
                    command: args.join(" "),
                    status: status
                        .code()
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "signal".into()),
                    detail: String::new(),
                });
            }
            Ok(CommandOutput::default())
        }
    }
}
