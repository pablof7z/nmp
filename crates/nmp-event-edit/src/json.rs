use std::ops::Range;

use serde::{de::IgnoredAny, Deserialize};

use crate::{EventEditPlan, JsonFieldEdit, JsonMissing, Occurrences, PlanError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonSpanPatch {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonEditOutcome {
    /// `None` means the exact source bytes already express the edit.
    pub replacement: Option<String>,
    pub patches: Vec<JsonSpanPatch>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub metrics: JsonApplyMetrics,
}

#[cfg(any(test, feature = "bench-instrumentation"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JsonApplyMetrics {
    pub source_bytes: usize,
    pub object_members: usize,
    /// Source bytes copied into a replacement candidate. A no-op remains
    /// zero: identity patches are discarded before any full-document copy.
    pub source_bytes_copied: usize,
    /// Escaped object keys that required transient decoding for semantic
    /// comparison. Ordinary unescaped keys are compared as borrowed slices.
    pub escaped_keys_decoded: usize,
    pub emitted_patches: usize,
    pub replacement_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonApplyError {
    InvalidPlan(PlanError),
    InvalidJson,
    NotAnObject,
    InvalidPatchSet,
}

impl From<PlanError> for JsonApplyError {
    fn from(value: PlanError) -> Self {
        Self::InvalidPlan(value)
    }
}

impl EventEditPlan {
    pub fn apply_json_object(&self, source: &str) -> Result<JsonEditOutcome, JsonApplyError> {
        apply_json_edit(self.json_edit()?, source)
    }
}

#[derive(Clone, Debug)]
struct Member {
    ordinal: usize,
    key_start: usize,
    value: Range<usize>,
    comma_before: Option<usize>,
    comma_after_end: Option<usize>,
}

struct ObjectSpans {
    /// Only members whose decoded key matches the requested field. The scan
    /// never retains one metadata row per unrelated object member.
    members: Vec<Member>,
    member_count: usize,
    close: usize,
    escaped_keys_decoded: usize,
}

fn apply_json_edit(edit: &JsonFieldEdit, source: &str) -> Result<JsonEditOutcome, JsonApplyError> {
    edit.validate()?;
    let (name, occurrences) = match edit {
        JsonFieldEdit::Set {
            name, occurrences, ..
        }
        | JsonFieldEdit::Remove {
            name, occurrences, ..
        } => (name, *occurrences),
    };
    let object = parse_object_spans(source, name)?;
    let matches = (0..object.members.len()).collect::<Vec<_>>();
    let selected = select_occurrences(&matches, occurrences);

    let mut patches = match edit {
        JsonFieldEdit::Set {
            value, if_missing, ..
        } => {
            if selected.is_empty() {
                match if_missing {
                    JsonMissing::NoChange => Vec::new(),
                    JsonMissing::Insert => vec![insert_field_patch(source, &object, name, value)?],
                }
            } else {
                selected
                    .iter()
                    .map(|index| {
                        let member = &object.members[*index];
                        JsonSpanPatch {
                            start: member.value.start,
                            end: member.value.end,
                            replacement: value.clone(),
                        }
                    })
                    .collect()
            }
        }
        JsonFieldEdit::Remove { .. } => removal_patches(&object, &selected),
    };

    // A Set to the exact existing JSON bytes is already the desired
    // document. Remove those identity patches before constructing a full
    // candidate; mixed patch sets remain equivalent because each dropped
    // range would have replaced bytes with themselves.
    patches.retain(|patch| source.get(patch.start..patch.end) != Some(patch.replacement.as_str()));

    let (replacement, source_bytes_copied) = if patches.is_empty() {
        (None, 0)
    } else {
        let (candidate, source_bytes_copied) = apply_patches(source, &patches)?;
        (Some(candidate), source_bytes_copied)
    };
    #[cfg(not(any(test, feature = "bench-instrumentation")))]
    let _ = (source_bytes_copied, object.escaped_keys_decoded);
    #[cfg(any(test, feature = "bench-instrumentation"))]
    let metrics = JsonApplyMetrics {
        source_bytes: source.len(),
        object_members: object.member_count,
        source_bytes_copied,
        escaped_keys_decoded: object.escaped_keys_decoded,
        emitted_patches: patches.len(),
        replacement_bytes: replacement.as_ref().map_or(0, String::len),
    };
    Ok(JsonEditOutcome {
        replacement,
        patches,
        #[cfg(any(test, feature = "bench-instrumentation"))]
        metrics,
    })
}

fn select_occurrences(matches: &[usize], occurrences: Occurrences) -> Vec<usize> {
    match occurrences {
        Occurrences::First => matches.first().copied().into_iter().collect(),
        Occurrences::Last => matches.last().copied().into_iter().collect(),
        Occurrences::All => matches.to_vec(),
    }
}

fn insert_field_patch(
    source: &str,
    object: &ObjectSpans,
    name: &str,
    value: &str,
) -> Result<JsonSpanPatch, JsonApplyError> {
    let encoded_name = serde_json::to_string(name).map_err(|_| JsonApplyError::InvalidJson)?;
    let separator = if object.member_count == 0 { "" } else { "," };
    let replacement = format!("{separator}{encoded_name}:{value}");
    if !source.is_char_boundary(object.close) {
        return Err(JsonApplyError::InvalidPatchSet);
    }
    Ok(JsonSpanPatch {
        start: object.close,
        end: object.close,
        replacement,
    })
}

fn removal_patches(object: &ObjectSpans, selected: &[usize]) -> Vec<JsonSpanPatch> {
    if selected.is_empty() {
        return Vec::new();
    }
    let mut patches = Vec::new();
    let mut run_start = 0;
    while run_start < selected.len() {
        let mut run_end = run_start + 1;
        while run_end < selected.len()
            && object.members[selected[run_end]].ordinal
                == object.members[selected[run_end - 1]].ordinal + 1
        {
            run_end += 1;
        }
        let first_index = selected[run_start];
        let last_index = selected[run_end - 1];
        let first = &object.members[first_index];
        let last = &object.members[last_index];
        let has_kept_after = last.ordinal + 1 < object.member_count;
        let (start, end) = if has_kept_after {
            (
                first.key_start,
                last.comma_after_end
                    .expect("a non-final valid object member has a comma"),
            )
        } else if let Some(comma) = first.comma_before {
            (comma, last.value.end)
        } else {
            (first.key_start, last.value.end)
        };
        patches.push(JsonSpanPatch {
            start,
            end,
            replacement: String::new(),
        });
        run_start = run_end;
    }
    patches
}

fn apply_patches(
    source: &str,
    patches: &[JsonSpanPatch],
) -> Result<(String, usize), JsonApplyError> {
    let mut order = (0..patches.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| (patches[*index].start, patches[*index].end));
    let mut cursor = 0;
    let mut output = String::with_capacity(
        source.len()
            + patches
                .iter()
                .map(|patch| patch.replacement.len())
                .sum::<usize>(),
    );
    let mut source_bytes_copied = 0;
    for index in order {
        let patch = &patches[index];
        if patch.start < cursor
            || patch.end < patch.start
            || patch.end > source.len()
            || !source.is_char_boundary(patch.start)
            || !source.is_char_boundary(patch.end)
        {
            return Err(JsonApplyError::InvalidPatchSet);
        }
        output.push_str(&source[cursor..patch.start]);
        source_bytes_copied += patch.start - cursor;
        output.push_str(&patch.replacement);
        cursor = patch.end;
    }
    output.push_str(&source[cursor..]);
    source_bytes_copied += source.len() - cursor;
    Ok((output, source_bytes_copied))
}

fn parse_object_spans(source: &str, name: &str) -> Result<ObjectSpans, JsonApplyError> {
    // Validate without materializing a parallel `serde_json::Value` tree. The
    // second pass below discovers exact source spans and is the only document
    // representation retained by this operation.
    let mut deserializer = serde_json::Deserializer::from_str(source);
    IgnoredAny::deserialize(&mut deserializer).map_err(|_| JsonApplyError::InvalidJson)?;
    deserializer
        .end()
        .map_err(|_| JsonApplyError::InvalidJson)?;

    let bytes = source.as_bytes();
    let mut at = skip_ws(bytes, 0);
    if bytes.get(at) != Some(&b'{') {
        return Err(JsonApplyError::NotAnObject);
    }
    at += 1;
    let mut members = Vec::new();
    let mut comma_before = None;
    let mut escaped_keys_decoded = 0;
    let mut member_count = 0;
    loop {
        at = skip_ws(bytes, at);
        if bytes.get(at) == Some(&b'}') {
            return Ok(ObjectSpans {
                members,
                member_count,
                close: at,
                escaped_keys_decoded,
            });
        }
        let key_start = at;
        let key_end = scan_string(bytes, at)?;
        let raw_key = &source[key_start + 1..key_end - 1];
        let matches_name = if raw_key.as_bytes().contains(&b'\\') {
            escaped_keys_decoded += 1;
            serde_json::from_str::<String>(&source[key_start..key_end])
                .map_err(|_| JsonApplyError::InvalidJson)?
                == name
        } else {
            raw_key == name
        };
        at = skip_ws(bytes, key_end);
        if bytes.get(at) != Some(&b':') {
            return Err(JsonApplyError::InvalidJson);
        }
        at = skip_ws(bytes, at + 1);
        let value_start = at;
        let value_end = scan_value(bytes, at)?;
        at = skip_ws(bytes, value_end);
        let comma_after_end = if bytes.get(at) == Some(&b',') {
            Some(at + 1)
        } else {
            None
        };
        if matches_name {
            members.push(Member {
                ordinal: member_count,
                key_start,
                value: value_start..value_end,
                comma_before,
                comma_after_end,
            });
        }
        member_count += 1;
        match bytes.get(at) {
            Some(b',') => {
                comma_before = Some(at);
                at += 1;
            }
            Some(b'}') => {
                return Ok(ObjectSpans {
                    members,
                    member_count,
                    close: at,
                    escaped_keys_decoded,
                });
            }
            _ => return Err(JsonApplyError::InvalidJson),
        }
    }
}

fn scan_string(bytes: &[u8], start: usize) -> Result<usize, JsonApplyError> {
    if bytes.get(start) != Some(&b'"') {
        return Err(JsonApplyError::InvalidJson);
    }
    let mut at = start + 1;
    while let Some(byte) = bytes.get(at) {
        match byte {
            b'"' => return Ok(at + 1),
            b'\\' => {
                at += 2;
            }
            _ => at += 1,
        }
    }
    Err(JsonApplyError::InvalidJson)
}

fn scan_value(bytes: &[u8], start: usize) -> Result<usize, JsonApplyError> {
    match bytes.get(start) {
        Some(b'"') => scan_string(bytes, start),
        Some(b'{') | Some(b'[') => scan_compound(bytes, start),
        Some(_) => {
            let mut at = start;
            while let Some(byte) = bytes.get(at) {
                if matches!(byte, b',' | b'}' | b' ' | b'\n' | b'\r' | b'\t') {
                    break;
                }
                at += 1;
            }
            (at > start)
                .then_some(at)
                .ok_or(JsonApplyError::InvalidJson)
        }
        None => Err(JsonApplyError::InvalidJson),
    }
}

fn scan_compound(bytes: &[u8], start: usize) -> Result<usize, JsonApplyError> {
    let mut stack = vec![bytes[start]];
    let mut at = start + 1;
    while let Some(byte) = bytes.get(at) {
        match byte {
            b'"' => at = scan_string(bytes, at)?,
            b'{' | b'[' => {
                stack.push(*byte);
                at += 1;
            }
            b'}' => {
                if stack.pop() != Some(b'{') {
                    return Err(JsonApplyError::InvalidJson);
                }
                at += 1;
                if stack.is_empty() {
                    return Ok(at);
                }
            }
            b']' => {
                if stack.pop() != Some(b'[') {
                    return Err(JsonApplyError::InvalidJson);
                }
                at += 1;
                if stack.is_empty() {
                    return Ok(at);
                }
            }
            _ => at += 1,
        }
    }
    Err(JsonApplyError::InvalidJson)
}

fn skip_ws(bytes: &[u8], mut at: usize) -> usize {
    while bytes
        .get(at)
        .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
    {
        at += 1;
    }
    at
}

#[cfg(test)]
#[path = "json/tests.rs"]
mod tests;
