use std::fmt;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Specified,
    Built,
    KnownViolation,
}

impl Status {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "specified" => Some(Self::Specified),
            "built" => Some(Self::Built),
            "known-violation" => Some(Self::KnownViolation),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Gap {
    Implementation,
    Evidence,
    Fixture,
    Platform,
}

impl Gap {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "implementation" => Some(Self::Implementation),
            "evidence" => Some(Self::Evidence),
            "fixture" => Some(Self::Fixture),
            "platform" => Some(Self::Platform),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Metadata {
    pub id: String,
    pub status: Status,
    pub evidence: Vec<String>,
    pub falsifier: Option<String>,
    pub gap: Option<Gap>,
    pub issue: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ScenarioRecord {
    pub file: PathBuf,
    pub line: usize,
    pub name: String,
    pub effective_tags: Vec<String>,
    pub metadata: Option<Metadata>,
    pub(crate) raw_metadata_present: bool,
    pub(crate) fingerprint: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TraceError(pub String);

impl fmt::Display for TraceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for TraceError {}

pub(crate) fn at(record: &ScenarioRecord, message: impl AsRef<str>) -> TraceError {
    TraceError(format!(
        "{}:{} (`{}`): {}",
        record.file.display(),
        record.line,
        record.name,
        message.as_ref()
    ))
}
