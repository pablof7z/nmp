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
            "--shape-corpus" => {
                config.shape_corpus = Some(path_value(&mut args, "--shape-corpus")?)
            }
            "--corpus-output" => {
                config.corpus_output = Some(path_value(&mut args, "--corpus-output")?)
            }
            "--memory-store" => config.memory_store = true,
            "--redb-nondurable-diagnostic" => config.redb_nondurable_diagnostic = true,
            "--queue-capacity" => config.queue_capacity = value(&mut args, "--queue-capacity")?,
            "--verified-cache-capacity" => {
                config.verified_cache_capacity = value(&mut args, "--verified-cache-capacity")?
            }
            "--committed-observation-cache-capacity" => {
                config.committed_observation_cache_capacity =
                    value(&mut args, "--committed-observation-cache-capacity")?
            }
            "--diagnostic-duplicate-ceiling-capacity" => {
                config.diagnostic_duplicate_ceiling_capacity =
                    value(&mut args, "--diagnostic-duplicate-ceiling-capacity")?
            }
            "--diagnostic-duplicate-ceiling-event-payload" => {
                config.diagnostic_duplicate_ceiling_event_payload = true
            }
            "--diagnostic-preparsed-ceiling" => config.diagnostic_preparsed_ceiling = true,
            "--diagnostic-skip-event-id-validation" => {
                config.diagnostic_skip_event_id_validation = true
            }
            "--diagnostic-skip-signature-verification" => {
                config.diagnostic_skip_signature_verification = true
            }
            "--verifier-workers" => {
                config.verifier_workers = value(&mut args, "--verifier-workers")?
            }
            "--verify-batch-size" => {
                config.verify_batch_size = value(&mut args, "--verify-batch-size")?
            }
            "--engine-batch-size" => {
                config.engine_batch_size = value(&mut args, "--engine-batch-size")?
            }
            "--engine-batch-bytes" => {
                config.engine_batch_bytes = value(&mut args, "--engine-batch-bytes")?
            }
            "--engine-batch-wait-us" => {
                config.engine_batch_wait =
                    Duration::from_micros(value(&mut args, "--engine-batch-wait-us")?)
            }
            "--visible-limit" => config.visible_limit = Some(value(&mut args, "--visible-limit")?),
            "--unlimited" => config.visible_limit = None,
            "--trim-allocator-during-ingest" => config.trim_allocator_during_ingest = true,
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
         --shape-corpus PATH generate the signed workload from a #620 private-free shape corpus\n\
         --corpus-output PATH retain the generated signed JSONL corpus\n\
         --memory-store       use the volatile semantic oracle as a no-persistence ceiling\n\
         --redb-nondurable-diagnostic\n\
                             benchmark nondurable foreground commits plus a timed final checkpoint\n\
         --queue-capacity N  every bounded runtime queue (default 1024)\n\
         --verified-cache-capacity N  verified ID/signature entries (default 131072)\n\
         --committed-observation-cache-capacity N\n\
                             durable exact-observation fast-path entries (default 131072)\n\
         --diagnostic-duplicate-ceiling-capacity N\n\
                             unsafe benchmark-only precommit exact-frame cache (default 0)\n\
         --diagnostic-duplicate-ceiling-event-payload\n\
                             fingerprint only the raw EVENT object, independent of subscription id\n\
         --diagnostic-preparsed-ceiling\n\
                             unsafe benchmark-only favorable ceiling; preload parsed frames\n\
         --diagnostic-skip-event-id-validation\n\
                             unsafe benchmark-only favorable ceiling; trust relay event IDs\n\
         --diagnostic-skip-signature-verification\n\
                             unsafe benchmark-only favorable ceiling; trust relay signatures\n\
         --verifier-workers N  native signature workers; 0 uses default 2 (maximum 16)\n\
         --verify-batch-size N  signature verification batch ceiling (default 128)\n\
         --engine-batch-size N  store transaction batch ceiling (default 128)\n\
         --engine-batch-bytes N  conservative encoded-byte ceiling (default 8388608)\n\
         --engine-batch-wait-us N  maximum EVENT coalescing wait (default 200)\n\
         --visible-limit N   live query window (default 200)\n\
         --unlimited         retain every matching row in the live query\n\
         --trim-allocator-during-ingest\n\
                             probe reclaimable glibc pages every 100 ms\n\
         --frame-delay-us N  pace each relay frame for soak runs\n\
         --expect-rejection  assert one oversize frame is rejected\n\
         --timeout-secs N    completion deadline (default 120)\n\
         --store PATH        retain the resulting redb store\n\
         --output PATH       write the JSON result in addition to stdout"
    );
}
