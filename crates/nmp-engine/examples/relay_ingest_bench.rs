#[path = "support/relay_ingest_probe.rs"]
mod relay_ingest_probe;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use relay_ingest_probe::{ProbeConfig, ProbeError};

fn main() -> Result<(), ProbeError> {
    let mut config = ProbeConfig::default();
    let mut output = None;
    let mut args = env::args_os().skip(1);
    while let Some(raw) = args.next() {
        let flag = raw.to_string_lossy();
        match flag.as_ref() {
            "--events" => config.events = value(&mut args, "--events")?,
            "--relays" => config.relays = value(&mut args, "--relays")?,
            "--passes" => config.passes = value(&mut args, "--passes")?,
            "--payload-bytes" => config.payload_bytes = value(&mut args, "--payload-bytes")?,
            "--queue-capacity" => config.queue_capacity = value(&mut args, "--queue-capacity")?,
            "--batch-size" => config.batch_size = value(&mut args, "--batch-size")?,
            "--visible-limit" => config.visible_limit = Some(value(&mut args, "--visible-limit")?),
            "--unlimited" => config.visible_limit = None,
            "--frame-delay-us" => {
                config.frame_delay = Duration::from_micros(value(&mut args, "--frame-delay-us")?)
            }
            "--expect-rejection" => config.expect_rejection = true,
            "--timeout-secs" => {
                config.timeout = Duration::from_secs(value(&mut args, "--timeout-secs")?)
            }
            "--store" => config.store_path = Some(path_value(&mut args, "--store")?),
            "--output" => output = Some(path_value(&mut args, "--output")?),
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }

    let result = relay_ingest_probe::run(config)?;
    let json = serde_json::to_string_pretty(&result)?;
    println!("{json}");
    if let Some(path) = output {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, format!("{json}\n"))?;
    }
    Ok(())
}

fn value<T: std::str::FromStr>(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    flag: &str,
) -> Result<T, ProbeError>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let raw = args
        .next()
        .ok_or_else(|| format!("{flag} requires a value"))?;
    Ok(raw.to_string_lossy().parse()?)
}

fn path_value(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    flag: &str,
) -> Result<PathBuf, ProbeError> {
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("{flag} requires a path").into())
}

fn print_help() {
    println!(
        "relay_ingest_bench [options]\n\
         \n\
         --events N          canonical event count (default 10000)\n\
         --relays N          independent websocket relays (default 1)\n\
         --passes N          full corpus replays per relay (default 1)\n\
         --payload-bytes N   event content bytes (default 128)\n\
         --queue-capacity N  every bounded runtime queue (default 1024)\n\
         --batch-size N      verify and engine batch ceiling (default 128)\n\
         --visible-limit N   live query window (default 200)\n\
         --unlimited         retain every matching row in the live query\n\
         --frame-delay-us N  pace each relay frame for soak runs\n\
         --expect-rejection  assert one oversize frame is rejected\n\
         --timeout-secs N    completion deadline (default 120)\n\
         --store PATH        retain the resulting redb store\n\
         --output PATH       write the JSON result in addition to stdout"
    );
}
