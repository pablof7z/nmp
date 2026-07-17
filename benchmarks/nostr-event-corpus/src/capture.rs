use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tungstenite::{connect, Message};

const SCHEMA: &str = "nmp-public-relay-capture-v1";

pub struct CaptureConfig {
    pub relay_name: String,
    pub relay_url: String,
    pub first_window: u64,
    pub end_exclusive: u64,
    pub stride_seconds: u64,
    pub window_seconds: u64,
    pub per_window_limit: usize,
    pub output: PathBuf,
}

#[derive(Serialize)]
struct CaptureManifest<'a> {
    schema: &'static str,
    relay_name: &'a str,
    relay_url: &'a str,
    first_window_unix: u64,
    end_exclusive_unix: u64,
    stride_seconds: u64,
    window_seconds: u64,
    requested_per_window_limit: usize,
    windows: Vec<WindowResult>,
    total_frames: u64,
    total_bytes: u64,
    elapsed_ms: f64,
}

#[derive(Serialize)]
struct WindowResult {
    start_unix: u64,
    end_exclusive_unix: u64,
    frames: u64,
    bytes: u64,
    blake3: String,
    earliest_created_at: Option<u64>,
    latest_created_at: Option<u64>,
    hit_requested_limit: bool,
}

pub fn run(config: CaptureConfig) -> Result<(), Box<dyn std::error::Error>> {
    validate(&config)?;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let relay_dir = config.output.join(&config.relay_name);
    fs::create_dir_all(&relay_dir)?;
    let started = Instant::now();
    let mut windows = Vec::new();

    let (mut socket, _) = connect(&config.relay_url)?;
    for start in
        (config.first_window..config.end_exclusive).step_by(usize::try_from(config.stride_seconds)?)
    {
        let end_exclusive = start
            .checked_add(config.window_seconds)
            .ok_or("window end overflow")?
            .min(config.end_exclusive);
        let subscription = format!("nmp620-{}-{start}", config.relay_name);
        let request = json!([
            "REQ",
            subscription,
            {
                "since": start,
                "until": end_exclusive - 1,
                "limit": config.per_window_limit,
            }
        ]);
        socket.send(Message::Text(request.to_string().into()))?;

        let path = relay_dir.join(format!("{start}.jsonl"));
        let mut output = BufWriter::new(File::create(path)?);
        let mut hasher = blake3::Hasher::new();
        let mut frames = 0_u64;
        let mut bytes = 0_u64;
        let mut earliest = None::<u64>;
        let mut latest = None::<u64>;

        loop {
            match socket.read()? {
                Message::Text(text) => {
                    let raw = text.as_str();
                    let value: Value = match serde_json::from_str(raw) {
                        Ok(value) => value,
                        Err(_) => continue,
                    };
                    let Some(kind) = value.get(0).and_then(Value::as_str) else {
                        continue;
                    };
                    let same_subscription = value
                        .get(1)
                        .and_then(Value::as_str)
                        .is_some_and(|candidate| candidate == subscription);
                    if kind == "EOSE" && same_subscription {
                        break;
                    }
                    if kind == "CLOSED" && same_subscription {
                        return Err(format!("relay closed {subscription}: {raw}").into());
                    }
                    if kind != "EVENT" || !same_subscription {
                        continue;
                    }

                    let created_at = value
                        .get(2)
                        .and_then(|event| event.get("created_at"))
                        .and_then(Value::as_u64)
                        .ok_or("EVENT frame lacks numeric created_at")?;
                    if !(start..end_exclusive).contains(&created_at) {
                        return Err(format!(
                            "relay returned created_at {created_at} outside [{start}, {end_exclusive})"
                        )
                        .into());
                    }
                    output.write_all(raw.as_bytes())?;
                    output.write_all(b"\n")?;
                    hasher.update(raw.as_bytes());
                    hasher.update(b"\n");
                    frames += 1;
                    bytes = bytes
                        .checked_add(u64::try_from(raw.len())?)
                        .ok_or("byte overflow")?;
                    earliest = Some(earliest.map_or(created_at, |old| old.min(created_at)));
                    latest = Some(latest.map_or(created_at, |old| old.max(created_at)));
                }
                Message::Ping(payload) => socket.send(Message::Pong(payload))?,
                Message::Close(frame) => {
                    return Err(format!("relay disconnected before EOSE: {frame:?}").into())
                }
                Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
        socket.send(Message::Text(
            json!(["CLOSE", subscription]).to_string().into(),
        ))?;
        output.flush()?;
        windows.push(WindowResult {
            start_unix: start,
            end_exclusive_unix: end_exclusive,
            frames,
            bytes,
            blake3: hasher.finalize().to_hex().to_string(),
            earliest_created_at: earliest,
            latest_created_at: latest,
            hit_requested_limit: frames >= u64::try_from(config.per_window_limit)?,
        });
    }
    let _ = socket.close(None);

    let total_frames = windows.iter().map(|window| window.frames).sum();
    let total_bytes = windows.iter().map(|window| window.bytes).sum();
    let manifest = CaptureManifest {
        schema: SCHEMA,
        relay_name: &config.relay_name,
        relay_url: &config.relay_url,
        first_window_unix: config.first_window,
        end_exclusive_unix: config.end_exclusive,
        stride_seconds: config.stride_seconds,
        window_seconds: config.window_seconds,
        requested_per_window_limit: config.per_window_limit,
        windows,
        total_frames,
        total_bytes,
        elapsed_ms: duration_ms(started.elapsed()),
    };
    let manifest_path = relay_dir.join("manifest.json");
    let mut file = BufWriter::new(File::create(manifest_path)?);
    serde_json::to_writer_pretty(&mut file, &manifest)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn validate(config: &CaptureConfig) -> Result<(), String> {
    if config.relay_name.is_empty()
        || !config
            .relay_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("relay-name must contain only ASCII letters, digits, or '-'".to_owned());
    }
    if config.first_window >= config.end_exclusive {
        return Err("first window must precede end-exclusive".to_owned());
    }
    if config.stride_seconds == 0 || config.window_seconds == 0 {
        return Err("stride and window must be nonzero".to_owned());
    }
    if config.window_seconds > config.stride_seconds {
        return Err("windows must not overlap".to_owned());
    }
    if config.per_window_limit == 0 {
        return Err("per-window limit must be nonzero".to_owned());
    }
    Ok(())
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
