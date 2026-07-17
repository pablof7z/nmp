mod analyze;
mod capture;

use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let command = args
        .next()
        .ok_or("usage: nmp-nostr-event-corpus <capture-relay|analyze> ...")?;

    match command.to_string_lossy().as_ref() {
        "capture-relay" => {
            let relay_name = string_arg(&mut args, "relay-name")?;
            let relay_url = string_arg(&mut args, "relay-url")?;
            let first_window = number_arg::<u64>(&mut args, "first-window-unix")?;
            let end_exclusive = number_arg::<u64>(&mut args, "end-exclusive-unix")?;
            let stride_seconds = number_arg::<u64>(&mut args, "stride-seconds")?;
            let window_seconds = number_arg::<u64>(&mut args, "window-seconds")?;
            let per_window_limit = number_arg::<usize>(&mut args, "per-window-limit")?;
            let output = path_arg(&mut args, "output-directory")?;
            capture::run(capture::CaptureConfig {
                relay_name,
                relay_url,
                first_window,
                end_exclusive,
                stride_seconds,
                window_seconds,
                per_window_limit,
                output,
            })?;
        }
        "analyze" => {
            let captures = path_arg(&mut args, "capture-directory")?;
            let target_shapes = number_arg::<usize>(&mut args, "target-shapes")?;
            let stats = path_arg(&mut args, "stats-output")?;
            let shapes = path_arg(&mut args, "shape-output")?;
            analyze::run(&captures, target_shapes, &stats, &shapes)?;
        }
        other => return Err(format!("unknown command {other}").into()),
    }

    if args.next().is_some() {
        return Err("unexpected trailing argument".into());
    }
    Ok(())
}

fn string_arg(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> Result<String, String> {
    args.next()
        .map(|value| value.to_string_lossy().into_owned())
        .ok_or_else(|| format!("missing {name}"))
}

fn path_arg(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> Result<PathBuf, String> {
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {name}"))
}

fn number_arg<T>(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    name: &str,
) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    args.next()
        .ok_or_else(|| format!("missing {name}"))?
        .to_string_lossy()
        .parse()
        .map_err(|error| format!("invalid {name}: {error}"))
}
