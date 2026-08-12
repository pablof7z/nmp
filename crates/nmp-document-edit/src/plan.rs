use serde::{de::IgnoredAny, Deserialize, Serialize};

/// A closed durable envelope. New persisted meanings require a new version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "version", content = "edit", rename_all = "snake_case")]
pub enum DocumentEditPlan {
    V1(DocumentEditV1),
}

impl DocumentEditPlan {
    pub fn tags(edit: TagEdit) -> Self {
        Self::V1(DocumentEditV1::Tags(edit))
    }

    pub fn json_object(edit: JsonFieldEdit) -> Self {
        Self::V1(DocumentEditV1::JsonObject(edit))
    }

    pub fn partitioned_tags(edit: PartitionedTagEdit) -> Result<Self, PlanError> {
        edit.validate()?;
        Ok(Self::V1(DocumentEditV1::PartitionedTags(edit)))
    }

    pub fn tag_edit(&self) -> Result<&TagEdit, PlanError> {
        match self {
            Self::V1(DocumentEditV1::Tags(edit)) => Ok(edit),
            _ => Err(PlanError::DocumentShapeMismatch),
        }
    }

    pub fn json_edit(&self) -> Result<&JsonFieldEdit, PlanError> {
        match self {
            Self::V1(DocumentEditV1::JsonObject(edit)) => Ok(edit),
            _ => Err(PlanError::DocumentShapeMismatch),
        }
    }

    pub fn partitioned_edit(&self) -> Result<&PartitionedTagEdit, PlanError> {
        match self {
            Self::V1(DocumentEditV1::PartitionedTags(edit)) => Ok(edit),
            _ => Err(PlanError::DocumentShapeMismatch),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "document", content = "operation", rename_all = "snake_case")]
pub enum DocumentEditV1 {
    Tags(TagEdit),
    JsonObject(JsonFieldEdit),
    PartitionedTags(PartitionedTagEdit),
}

/// One logical item may use one or more consecutive tag rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagItemPattern {
    rows: Vec<TagRowPattern>,
}

impl TagItemPattern {
    pub fn new(rows: Vec<TagRowPattern>) -> Result<Self, PlanError> {
        if rows.is_empty() {
            return Err(PlanError::EmptyItemPattern);
        }
        Ok(Self { rows })
    }

    pub fn rows(&self) -> &[TagRowPattern] {
        &self.rows
    }

    pub(crate) fn validate(&self) -> Result<(), PlanError> {
        if self.rows.is_empty() {
            return Err(PlanError::EmptyItemPattern);
        }
        self.rows.iter().try_for_each(TagRowPattern::validate)
    }
}

/// Match one row by exact leading cells, optionally requiring its exact size.
///
/// Leaving `exact_cell_count` unset deliberately preserves legacy extra cells.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagRowPattern {
    prefix: Vec<String>,
    exact_cell_count: Option<usize>,
}

impl TagRowPattern {
    pub fn prefix(prefix: Vec<String>) -> Result<Self, PlanError> {
        Self::new(prefix, None)
    }

    pub fn exact(prefix: Vec<String>, cell_count: usize) -> Result<Self, PlanError> {
        Self::new(prefix, Some(cell_count))
    }

    fn new(prefix: Vec<String>, exact_cell_count: Option<usize>) -> Result<Self, PlanError> {
        let value = Self {
            prefix,
            exact_cell_count,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn cells(&self) -> &[String] {
        &self.prefix
    }

    pub fn exact_cell_count(&self) -> Option<usize> {
        self.exact_cell_count
    }

    pub(crate) fn validate(&self) -> Result<(), PlanError> {
        if self.prefix.is_empty() {
            return Err(PlanError::EmptyRowPattern);
        }
        if self
            .exact_cell_count
            .is_some_and(|count| count < self.prefix.len())
        {
            return Err(PlanError::ExactRowShorterThanPrefix);
        }
        Ok(())
    }
}

/// Ordered alternatives let a capability recognize current and legacy forms.
/// The first matching alternative wins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagItemSelector {
    alternatives: Vec<TagItemPattern>,
}

impl TagItemSelector {
    pub fn one(pattern: TagItemPattern) -> Self {
        Self {
            alternatives: vec![pattern],
        }
    }

    pub fn new(alternatives: Vec<TagItemPattern>) -> Result<Self, PlanError> {
        if alternatives.is_empty() {
            return Err(PlanError::EmptyItemSelector);
        }
        let value = Self { alternatives };
        value.validate()?;
        Ok(value)
    }

    pub fn alternatives(&self) -> &[TagItemPattern] {
        &self.alternatives
    }

    pub(crate) fn validate(&self) -> Result<(), PlanError> {
        if self.alternatives.is_empty() {
            return Err(PlanError::EmptyItemSelector);
        }
        self.alternatives
            .iter()
            .try_for_each(TagItemPattern::validate)
    }
}

/// A primitive over capability-selected logical items.
///
/// Higher-level add/remove/reorder/reset/migrate verbs remain capability
/// decisions. They compile to these three replayable structural meanings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum TagEdit {
    /// Keep an existing legacy/current representation; insert the current
    /// encoding only when no selected item exists.
    EnsurePresent {
        selector: TagItemSelector,
        rows: Vec<Vec<String>>,
        insertion: TagInsertion,
    },
    /// Remove every selected logical item, including duplicate encodings.
    Remove { selector: TagItemSelector },
    /// Remove every item selected by any selector and insert one capability-
    /// encoded replacement. Empty `rows` expresses a capability-defined clear,
    /// not a protocol-wide reset/delete alias.
    Rewrite {
        selectors: Vec<TagItemSelector>,
        rows: Vec<Vec<String>>,
        insertion: TagInsertion,
    },
}

impl TagEdit {
    pub fn ensure_present(
        selector: TagItemSelector,
        rows: Vec<Vec<String>>,
        insertion: TagInsertion,
    ) -> Result<Self, PlanError> {
        if rows.is_empty() {
            return Err(PlanError::EmptyInsertedItem);
        }
        let value = Self::EnsurePresent {
            selector,
            rows,
            insertion,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn remove(selector: TagItemSelector) -> Self {
        Self::Remove { selector }
    }

    pub fn rewrite(
        selectors: Vec<TagItemSelector>,
        rows: Vec<Vec<String>>,
        insertion: TagInsertion,
    ) -> Result<Self, PlanError> {
        if selectors.is_empty() {
            return Err(PlanError::EmptyRewriteSelector);
        }
        let value = Self::Rewrite {
            selectors,
            rows,
            insertion,
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn validate(&self) -> Result<(), PlanError> {
        match self {
            Self::EnsurePresent {
                selector,
                rows,
                insertion,
            } => {
                selector.validate()?;
                validate_rows(rows, false)?;
                insertion.validate()
            }
            Self::Remove { selector } => selector.validate(),
            Self::Rewrite {
                selectors,
                rows,
                insertion,
            } => {
                if selectors.is_empty() {
                    return Err(PlanError::EmptyRewriteSelector);
                }
                selectors.iter().try_for_each(TagItemSelector::validate)?;
                validate_rows(rows, true)?;
                insertion.validate()
            }
        }
    }
}

fn validate_rows(rows: &[Vec<String>], empty_allowed: bool) -> Result<(), PlanError> {
    if !empty_allowed && rows.is_empty() {
        return Err(PlanError::EmptyInsertedItem);
    }
    if rows.iter().any(Vec::is_empty) {
        return Err(PlanError::EmptyInsertedRow);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "position", rename_all = "snake_case")]
pub enum TagInsertion {
    Boundary {
        boundary: Boundary,
    },
    FirstMatchOr {
        fallback: Boundary,
    },
    BeforeFirst {
        anchor: TagItemSelector,
        fallback: Boundary,
    },
    AfterLast {
        anchor: TagItemSelector,
        fallback: Boundary,
    },
}

impl TagInsertion {
    pub const fn start() -> Self {
        Self::Boundary {
            boundary: Boundary::Start,
        }
    }

    pub const fn end() -> Self {
        Self::Boundary {
            boundary: Boundary::End,
        }
    }

    pub const fn first_match_or(fallback: Boundary) -> Self {
        Self::FirstMatchOr { fallback }
    }

    pub fn before_first(anchor: TagItemSelector, fallback: Boundary) -> Self {
        Self::BeforeFirst { anchor, fallback }
    }

    pub fn after_last(anchor: TagItemSelector, fallback: Boundary) -> Self {
        Self::AfterLast { anchor, fallback }
    }

    pub(crate) fn validate(&self) -> Result<(), PlanError> {
        match self {
            Self::BeforeFirst { anchor, .. } | Self::AfterLast { anchor, .. } => anchor.validate(),
            Self::Boundary { .. } | Self::FirstMatchOr { .. } => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    Start,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Occurrences {
    First,
    Last,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonMissing {
    Insert,
    NoChange,
}

/// One field-level JSON-object edit. Values are retained as exact JSON bytes
/// and validated before a span patch is produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum JsonFieldEdit {
    Set {
        name: String,
        value: String,
        occurrences: Occurrences,
        if_missing: JsonMissing,
    },
    Remove {
        name: String,
        occurrences: Occurrences,
    },
}

impl JsonFieldEdit {
    pub fn set(
        name: impl Into<String>,
        value: impl Into<String>,
        occurrences: Occurrences,
        if_missing: JsonMissing,
    ) -> Result<Self, PlanError> {
        let value = Self::Set {
            name: name.into(),
            value: value.into(),
            occurrences,
            if_missing,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn remove(name: impl Into<String>, occurrences: Occurrences) -> Result<Self, PlanError> {
        let value = Self::Remove {
            name: name.into(),
            occurrences,
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn validate(&self) -> Result<(), PlanError> {
        let (name, raw_value) = match self {
            Self::Set { name, value, .. } => (name, Some(value)),
            Self::Remove { name, .. } => (name, None),
        };
        if name.is_empty() {
            return Err(PlanError::EmptyJsonFieldName);
        }
        if let Some(value) = raw_value {
            let mut deserializer = serde_json::Deserializer::from_str(value);
            IgnoredAny::deserialize(&mut deserializer).map_err(|_| PlanError::InvalidJsonValue)?;
            deserializer
                .end()
                .map_err(|_| PlanError::InvalidJsonValue)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Partition {
    Public,
    Private,
}

/// Independent structural operations for two representable partitions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionedTagEdit {
    public: Option<TagEdit>,
    private: Option<TagEdit>,
}

impl PartitionedTagEdit {
    pub fn new(public: Option<TagEdit>, private: Option<TagEdit>) -> Result<Self, PlanError> {
        let value = Self { public, private };
        value.validate()?;
        Ok(value)
    }

    pub fn only(partition: Partition, edit: TagEdit) -> Self {
        match partition {
            Partition::Public => Self {
                public: Some(edit),
                private: None,
            },
            Partition::Private => Self {
                public: None,
                private: Some(edit),
            },
        }
    }

    pub fn public(&self) -> Option<&TagEdit> {
        self.public.as_ref()
    }

    pub fn private(&self) -> Option<&TagEdit> {
        self.private.as_ref()
    }

    pub(crate) fn validate(&self) -> Result<(), PlanError> {
        if self.public.is_none() && self.private.is_none() {
            return Err(PlanError::EmptyPartitionedEdit);
        }
        if let Some(edit) = &self.public {
            edit.validate()?;
        }
        if let Some(edit) = &self.private {
            edit.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    DocumentShapeMismatch,
    EmptyItemPattern,
    EmptyRowPattern,
    ExactRowShorterThanPrefix,
    EmptyItemSelector,
    EmptyInsertedItem,
    EmptyInsertedRow,
    EmptyRewriteSelector,
    EmptyJsonFieldName,
    InvalidJsonValue,
    EmptyPartitionedEdit,
}
