use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use nostr::{Event, JsonUtil};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const STATS_SCHEMA: &str = "nmp-public-event-distribution-v1";
const SHAPES_SCHEMA: &str = "nmp-private-free-event-shapes-v1";
const TINY_MAX_FRAME_BYTES: usize = 4 * 1024;
const FRAME_THRESHOLDS: [usize; 6] = [256, 512, 1024, 2048, 4096, 16384];

#[derive(Deserialize)]
struct RelayManifest {
    relay_name: String,
    relay_url: String,
    first_window_unix: u64,
    end_exclusive_unix: u64,
    stride_seconds: u64,
    window_seconds: u64,
    requested_per_window_limit: usize,
    windows: Vec<WindowManifest>,
}

#[derive(Deserialize)]
struct WindowManifest {
    start_unix: u64,
    end_exclusive_unix: u64,
    frames: u64,
    bytes: u64,
    blake3: String,
    hit_requested_limit: bool,
}

#[derive(Debug, Clone)]
struct EventMetrics {
    frame_bytes: usize,
    event_json_bytes: usize,
    content_bytes: usize,
    tag_count: usize,
    tag_atom_count: usize,
    encoded_tag_bytes: usize,
    kind: u16,
}

#[derive(Debug, Clone)]
struct Candidate {
    id: String,
    metrics: EventMetrics,
    content: StringShape,
    tags: Vec<TagShape>,
}

#[derive(Default)]
struct Population {
    frame_bytes: Vec<usize>,
    event_json_bytes: Vec<usize>,
    content_bytes: Vec<usize>,
    tag_counts: Vec<usize>,
    tag_atom_counts: Vec<usize>,
    encoded_tag_bytes: Vec<usize>,
    kinds: BTreeMap<u16, u64>,
}

impl Population {
    fn push(&mut self, metrics: &EventMetrics) {
        self.frame_bytes.push(metrics.frame_bytes);
        self.event_json_bytes.push(metrics.event_json_bytes);
        self.content_bytes.push(metrics.content_bytes);
        self.tag_counts.push(metrics.tag_count);
        self.tag_atom_counts.push(metrics.tag_atom_count);
        self.encoded_tag_bytes.push(metrics.encoded_tag_bytes);
        *self.kinds.entry(metrics.kind).or_default() += 1;
    }

    fn report(mut self) -> PopulationReport {
        let total = self.frame_bytes.len();
        PopulationReport {
            events: u64::try_from(total).expect("event count fits u64"),
            frame_bytes: Distribution::from_values(&mut self.frame_bytes),
            event_json_bytes: Distribution::from_values(&mut self.event_json_bytes),
            decoded_content_bytes: Distribution::from_values(&mut self.content_bytes),
            tag_count: Distribution::from_values(&mut self.tag_counts),
            tag_atom_count: Distribution::from_values(&mut self.tag_atom_counts),
            encoded_tag_bytes: Distribution::from_values(&mut self.encoded_tag_bytes),
            frame_thresholds: FRAME_THRESHOLDS
                .iter()
                .map(|threshold| ThresholdShare {
                    at_most_bytes: *threshold,
                    events: u64::try_from(
                        self.frame_bytes
                            .iter()
                            .filter(|value| **value <= *threshold)
                            .count(),
                    )
                    .expect("threshold count fits u64"),
                    proportion: proportion(
                        self.frame_bytes
                            .iter()
                            .filter(|value| **value <= *threshold)
                            .count(),
                        total,
                    ),
                })
                .collect(),
            kinds: sorted_kinds(self.kinds, total),
        }
    }
}

#[derive(Serialize)]
struct AnalysisReport {
    schema: &'static str,
    source_capture_blake3: String,
    relays: Vec<RelayReport>,
    first_window_unix: u64,
    end_exclusive_unix: u64,
    stride_seconds: u64,
    window_seconds: u64,
    requested_per_window_limit: usize,
    windows: u64,
    windows_hitting_requested_limit: u64,
    raw_frames: u64,
    valid_event_observations: u64,
    unique_valid_events: u64,
    duplicate_observations: u64,
    duplicate_rate: f64,
    malformed_frames: u64,
    invalid_events: u64,
    conflicting_event_ids: u64,
    observations: PopulationReport,
    unique_events: PopulationReport,
    tiny_definition: String,
    tiny_unique_events: u64,
    selected_private_free_shapes: u64,
}

#[derive(Serialize)]
struct RelayReport {
    name: String,
    url: String,
    frames: u64,
    valid_event_observations: u64,
    unique_event_ids: u64,
}

#[derive(Serialize)]
struct PopulationReport {
    events: u64,
    frame_bytes: Distribution,
    event_json_bytes: Distribution,
    decoded_content_bytes: Distribution,
    tag_count: Distribution,
    tag_atom_count: Distribution,
    encoded_tag_bytes: Distribution,
    frame_thresholds: Vec<ThresholdShare>,
    kinds: Vec<KindCount>,
}

#[derive(Serialize)]
struct Distribution {
    p50: usize,
    p75: usize,
    p90: usize,
    p95: usize,
    p99: usize,
    p99_9: usize,
    max: usize,
}

impl Distribution {
    fn from_values(values: &mut [usize]) -> Self {
        values.sort_unstable();
        Self {
            p50: percentile(values, 500),
            p75: percentile(values, 750),
            p90: percentile(values, 900),
            p95: percentile(values, 950),
            p99: percentile(values, 990),
            p99_9: percentile(values, 999),
            max: values.last().copied().unwrap_or(0),
        }
    }
}

#[derive(Serialize)]
struct ThresholdShare {
    at_most_bytes: usize,
    events: u64,
    proportion: f64,
}

#[derive(Serialize)]
struct KindCount {
    kind: u16,
    events: u64,
    proportion: f64,
}

#[derive(Debug, Clone, Serialize)]
struct StringShape {
    utf8_bytes: usize,
    json_bytes: usize,
    class: StringClass,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum StringClass {
    Empty,
    LowerHex64,
    Url,
    Plain,
}

#[derive(Debug, Clone, Serialize)]
struct TagShape {
    name: TagNameShape,
    values: Vec<StringShape>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "class", rename_all = "snake_case")]
enum TagNameShape {
    PublicProtocol {
        value: String,
    },
    SingleLetter {
        value: char,
    },
    Synthetic {
        utf8_bytes: usize,
        json_bytes: usize,
    },
}

#[derive(Serialize)]
struct ShapeCorpus {
    schema: &'static str,
    source_capture_blake3: String,
    tiny_max_frame_bytes: usize,
    source_tiny_unique_events: u64,
    target_shapes: usize,
    actual_shapes: usize,
    sampling: &'static str,
    privacy_boundary: &'static str,
    shapes: Vec<SelectedShape>,
}

#[derive(Serialize)]
struct SelectedShape {
    kind: u16,
    observed_frame_bytes: usize,
    observed_event_json_bytes: usize,
    content: StringShape,
    tags: Vec<TagShape>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Stratum {
    kind: u16,
    size_bucket: u8,
    tag_bucket: u8,
}

pub fn run(
    capture_root: &Path,
    target_shapes: usize,
    stats_path: &Path,
    shapes_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if target_shapes == 0 {
        return Err("target-shapes must be nonzero".into());
    }
    let mut relay_dirs = fs::read_dir(capture_root)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    relay_dirs.sort();
    if relay_dirs.is_empty() {
        return Err("capture directory contains no relay directories".into());
    }

    let mut source_hasher = blake3::Hasher::new();
    let mut observations = Population::default();
    let mut unique_population = Population::default();
    let mut unique = BTreeMap::<String, Candidate>::new();
    let mut relay_reports = Vec::new();
    let mut first_window = u64::MAX;
    let mut end_exclusive = 0_u64;
    let mut common_stride = None;
    let mut common_window = None;
    let mut common_limit = None;
    let mut total_windows = 0_u64;
    let mut saturated_windows = 0_u64;
    let mut raw_frames = 0_u64;
    let mut valid_observations = 0_u64;
    let mut malformed = 0_u64;
    let mut invalid = 0_u64;
    let mut conflicts = 0_u64;

    for relay_dir in relay_dirs {
        let manifest_bytes = fs::read(relay_dir.join("manifest.json"))?;
        source_hasher.update(&manifest_bytes);
        let manifest: RelayManifest = serde_json::from_slice(&manifest_bytes)?;
        bind_common(&mut common_stride, manifest.stride_seconds, "stride")?;
        bind_common(&mut common_window, manifest.window_seconds, "window")?;
        bind_common(
            &mut common_limit,
            manifest.requested_per_window_limit,
            "per-window limit",
        )?;
        first_window = first_window.min(manifest.first_window_unix);
        end_exclusive = end_exclusive.max(manifest.end_exclusive_unix);
        total_windows += u64::try_from(manifest.windows.len())?;
        saturated_windows += u64::try_from(
            manifest
                .windows
                .iter()
                .filter(|window| window.hit_requested_limit)
                .count(),
        )?;

        let mut relay_frames = 0_u64;
        let mut relay_valid = 0_u64;
        let mut relay_ids = BTreeSet::new();
        for window in &manifest.windows {
            let path = relay_dir.join(format!("{}.jsonl", window.start_unix));
            let file = File::open(&path)?;
            let mut hasher = blake3::Hasher::new();
            let mut actual_frames = 0_u64;
            let mut actual_bytes = 0_u64;
            for line in BufReader::new(file).lines() {
                let line = line?;
                hasher.update(line.as_bytes());
                hasher.update(b"\n");
                source_hasher.update(manifest.relay_name.as_bytes());
                source_hasher.update(&window.start_unix.to_le_bytes());
                source_hasher.update(line.as_bytes());
                source_hasher.update(b"\n");
                actual_frames += 1;
                actual_bytes += u64::try_from(line.len())?;
                raw_frames += 1;
                relay_frames += 1;

                let Some((id, candidate)) = parse_candidate(&line) else {
                    malformed += 1;
                    continue;
                };
                let event_json = frame_event_json(&line)?;
                let event = match Event::from_json(&event_json) {
                    Ok(event) if event.verify().is_ok() => event,
                    _ => {
                        invalid += 1;
                        continue;
                    }
                };
                if event.id.to_hex() != id {
                    invalid += 1;
                    continue;
                }
                observations.push(&candidate.metrics);
                valid_observations += 1;
                relay_valid += 1;
                relay_ids.insert(id.clone());
                match unique.entry(id) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        unique_population.push(&candidate.metrics);
                        entry.insert(candidate);
                    }
                    std::collections::btree_map::Entry::Occupied(entry) => {
                        if !same_shape(entry.get(), &candidate) {
                            conflicts += 1;
                        }
                    }
                }
            }
            if actual_frames != window.frames
                || actual_bytes != window.bytes
                || hasher.finalize().to_hex().as_str() != window.blake3
            {
                return Err(format!("capture manifest mismatch: {}", path.display()).into());
            }
            if window.end_exclusive_unix <= window.start_unix {
                return Err(format!("invalid window in {}", path.display()).into());
            }
        }
        relay_reports.push(RelayReport {
            name: manifest.relay_name,
            url: manifest.relay_url,
            frames: relay_frames,
            valid_event_observations: relay_valid,
            unique_event_ids: u64::try_from(relay_ids.len())?,
        });
    }

    let source_hash = source_hasher.finalize().to_hex().to_string();
    let tiny = unique
        .values()
        .filter(|candidate| candidate.metrics.frame_bytes <= TINY_MAX_FRAME_BYTES)
        .cloned()
        .collect::<Vec<_>>();
    let selected = select_shapes(&tiny, target_shapes);
    let shape_corpus = ShapeCorpus {
        schema: SHAPES_SCHEMA,
        source_capture_blake3: source_hash.clone(),
        tiny_max_frame_bytes: TINY_MAX_FRAME_BYTES,
        source_tiny_unique_events: u64::try_from(tiny.len())?,
        target_shapes,
        actual_shapes: selected.len(),
        sampling: "proportional largest-remainder quotas over kind x frame-size-bucket x tag-count-bucket; deterministic event-id order within each stratum",
        privacy_boundary: "contains only byte counts, JSON-encoded byte counts, public protocol tag-name classes, and coarse value classes; no event ids, pubkeys, signatures, content, or tag values",
        shapes: selected,
    };
    write_json(shapes_path, &shape_corpus)?;

    let duplicate_observations = valid_observations.saturating_sub(u64::try_from(unique.len())?);
    let report = AnalysisReport {
        schema: STATS_SCHEMA,
        source_capture_blake3: source_hash,
        relays: relay_reports,
        first_window_unix: first_window,
        end_exclusive_unix: end_exclusive,
        stride_seconds: common_stride.ok_or("missing stride")?,
        window_seconds: common_window.ok_or("missing window")?,
        requested_per_window_limit: common_limit.ok_or("missing limit")?,
        windows: total_windows,
        windows_hitting_requested_limit: saturated_windows,
        raw_frames,
        valid_event_observations: valid_observations,
        unique_valid_events: u64::try_from(unique.len())?,
        duplicate_observations,
        duplicate_rate: if valid_observations == 0 {
            0.0
        } else {
            duplicate_observations as f64 / valid_observations as f64
        },
        malformed_frames: malformed,
        invalid_events: invalid,
        conflicting_event_ids: conflicts,
        observations: observations.report(),
        unique_events: unique_population.report(),
        tiny_definition: format!("raw WebSocket EVENT frame <= {TINY_MAX_FRAME_BYTES} bytes"),
        tiny_unique_events: u64::try_from(tiny.len())?,
        selected_private_free_shapes: u64::try_from(shape_corpus.actual_shapes)?,
    };
    write_json(stats_path, &report)?;
    Ok(())
}

fn parse_candidate(frame: &str) -> Option<(String, Candidate)> {
    let value: Value = serde_json::from_str(frame).ok()?;
    let array = value.as_array()?;
    if array.first()?.as_str()? != "EVENT" {
        return None;
    }
    let event = array.get(2)?.as_object()?;
    let id = event.get("id")?.as_str()?.to_owned();
    let kind = u16::try_from(event.get("kind")?.as_u64()?).ok()?;
    let content = event.get("content")?.as_str()?;
    let tags_value = event.get("tags")?;
    let tags = tags_value.as_array()?;
    let tag_shapes = tags.iter().map(tag_shape).collect::<Option<Vec<_>>>()?;
    let tag_atom_count = tags
        .iter()
        .map(|tag| tag.as_array().map(Vec::len))
        .sum::<Option<usize>>()?;
    let event_json_bytes = serde_json::to_vec(event).ok()?.len();
    let encoded_tag_bytes = serde_json::to_vec(tags_value).ok()?.len();
    let metrics = EventMetrics {
        frame_bytes: frame.len(),
        event_json_bytes,
        content_bytes: content.len(),
        tag_count: tags.len(),
        tag_atom_count,
        encoded_tag_bytes,
        kind,
    };
    Some((
        id.clone(),
        Candidate {
            id,
            metrics,
            content: string_shape(content),
            tags: tag_shapes,
        },
    ))
}

fn frame_event_json(frame: &str) -> Result<String, serde_json::Error> {
    let value: Value = serde_json::from_str(frame)?;
    serde_json::to_string(&value[2])
}

fn tag_shape(value: &Value) -> Option<TagShape> {
    let atoms = value.as_array()?;
    let name = atoms.first()?.as_str()?;
    let name = if is_public_protocol_tag(name) {
        TagNameShape::PublicProtocol {
            value: name.to_owned(),
        }
    } else if name.len() == 1 && name.is_ascii() {
        TagNameShape::SingleLetter {
            value: name.chars().next()?,
        }
    } else {
        let shape = string_shape(name);
        TagNameShape::Synthetic {
            utf8_bytes: shape.utf8_bytes,
            json_bytes: shape.json_bytes,
        }
    };
    let values = atoms
        .iter()
        .skip(1)
        .map(|atom| atom.as_str().map(string_shape))
        .collect::<Option<Vec<_>>>()?;
    Some(TagShape { name, values })
}

fn string_shape(value: &str) -> StringShape {
    let class = if value.is_empty() {
        StringClass::Empty
    } else if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        StringClass::LowerHex64
    } else if value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("ws://")
        || value.starts_with("wss://")
    {
        StringClass::Url
    } else {
        StringClass::Plain
    };
    StringShape {
        utf8_bytes: value.len(),
        json_bytes: serde_json::to_string(value)
            .expect("string serialization cannot fail")
            .len(),
        class,
    }
}

fn is_public_protocol_tag(name: &str) -> bool {
    matches!(
        name,
        "a" | "d"
            | "e"
            | "h"
            | "i"
            | "k"
            | "l"
            | "p"
            | "q"
            | "r"
            | "t"
            | "alt"
            | "client"
            | "emoji"
            | "expiration"
            | "imeta"
            | "nonce"
            | "proxy"
            | "relays"
            | "subject"
            | "title"
    )
}

fn same_shape(left: &Candidate, right: &Candidate) -> bool {
    left.metrics.kind == right.metrics.kind
        && left.metrics.event_json_bytes == right.metrics.event_json_bytes
        && left.metrics.content_bytes == right.metrics.content_bytes
        && left.metrics.tag_count == right.metrics.tag_count
        && left.metrics.encoded_tag_bytes == right.metrics.encoded_tag_bytes
}

fn select_shapes(candidates: &[Candidate], target: usize) -> Vec<SelectedShape> {
    let target = target.min(candidates.len());
    if target == 0 {
        return Vec::new();
    }
    let mut strata = BTreeMap::<Stratum, Vec<&Candidate>>::new();
    for candidate in candidates {
        strata
            .entry(stratum(candidate))
            .or_default()
            .push(candidate);
    }
    for values in strata.values_mut() {
        values.sort_by(|left, right| left.id.cmp(&right.id));
    }

    let total = candidates.len();
    let mut quotas = BTreeMap::<Stratum, usize>::new();
    let mut remainders = Vec::new();
    let mut assigned = 0_usize;
    for (key, values) in &strata {
        let numerator = values.len() * target;
        let base = numerator / total;
        quotas.insert(*key, base);
        assigned += base;
        remainders.push((numerator % total, *key));
    }
    remainders.sort_by(|left, right| right.cmp(left));
    for (_, key) in remainders.into_iter().take(target - assigned) {
        *quotas.get_mut(&key).expect("quota exists") += 1;
    }

    let mut selected = Vec::with_capacity(target);
    for (key, values) in strata {
        let quota = quotas[&key];
        for index in 0..quota {
            let source_index = ((2 * index + 1) * values.len()) / (2 * quota);
            let candidate = values[source_index.min(values.len() - 1)];
            selected.push(SelectedShape {
                kind: candidate.metrics.kind,
                observed_frame_bytes: candidate.metrics.frame_bytes,
                observed_event_json_bytes: candidate.metrics.event_json_bytes,
                content: candidate.content.clone(),
                tags: candidate.tags.clone(),
            });
        }
    }
    selected.sort_by_key(|shape| {
        (
            shape.kind,
            shape.observed_frame_bytes,
            shape.observed_event_json_bytes,
        )
    });
    selected
}

fn stratum(candidate: &Candidate) -> Stratum {
    Stratum {
        kind: candidate.metrics.kind,
        size_bucket: size_bucket(candidate.metrics.frame_bytes),
        tag_bucket: match candidate.metrics.tag_count {
            0 => 0,
            1 => 1,
            2..=3 => 2,
            4..=7 => 3,
            _ => 4,
        },
    }
}

fn size_bucket(bytes: usize) -> u8 {
    match bytes {
        0..=256 => 0,
        257..=512 => 1,
        513..=1024 => 2,
        1025..=2048 => 3,
        2049..=4096 => 4,
        4097..=16384 => 5,
        _ => 6,
    }
}

fn percentile(values: &[usize], per_mille: usize) -> usize {
    if values.is_empty() {
        return 0;
    }
    let rank = (values.len() * per_mille).div_ceil(1000);
    values[rank.saturating_sub(1).min(values.len() - 1)]
}

fn proportion(part: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64
    }
}

fn sorted_kinds(kinds: BTreeMap<u16, u64>, total: usize) -> Vec<KindCount> {
    let mut kinds = kinds
        .into_iter()
        .map(|(kind, events)| KindCount {
            kind,
            events,
            proportion: if total == 0 {
                0.0
            } else {
                events as f64 / total as f64
            },
        })
        .collect::<Vec<_>>();
    kinds.sort_by(|left, right| {
        right
            .events
            .cmp(&left.events)
            .then_with(|| left.kind.cmp(&right.kind))
    });
    kinds
}

fn bind_common<T: Copy + PartialEq>(
    slot: &mut Option<T>,
    value: T,
    label: &str,
) -> Result<(), String> {
    match slot {
        Some(old) if *old != value => Err(format!("relay manifests disagree on {label}")),
        Some(_) => Ok(()),
        None => {
            *slot = Some(value);
            Ok(())
        }
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = BufWriter::new(File::create(path)?);
    serde_json::to_writer_pretty(&mut output, value)?;
    output.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles_are_exact() {
        let values = (1..=1_000).collect::<Vec<_>>();
        assert_eq!(percentile(&values, 500), 500);
        assert_eq!(percentile(&values, 999), 999);
    }

    #[test]
    fn string_shape_retains_cost_but_not_value() {
        let secret = "private value\nwith escaping";
        let encoded = serde_json::to_string(secret).unwrap();
        let shape = string_shape(secret);
        assert_eq!(shape.utf8_bytes, secret.len());
        assert_eq!(shape.json_bytes, encoded.len());
        let serialized = serde_json::to_string(&shape).unwrap();
        assert!(!serialized.contains("private"));
        assert!(!serialized.contains("escaping"));
    }
}
