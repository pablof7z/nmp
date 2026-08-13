use std::ops::Range;

use crate::{
    Boundary, EventEditPlan, PartitionedTagEdit, PlanError, TagEdit, TagInsertion, TagItemPattern,
    TagItemSelector, TagRowPattern,
};

mod consuming;
pub use consuming::{ConsumingPartitionedTagEditOutcome, ConsumingTagEditOutcome};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagEditOutcome {
    /// `None` means the input is already the desired document and no rows were
    /// rebuilt. `Some` is one single-pass reconstruction.
    pub replacement: Option<Vec<Vec<String>>>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub metrics: TagApplyMetrics,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionedTagEditOutcome {
    pub public: TagEditOutcome,
    pub private: TagEditOutcome,
}

#[cfg(any(test, feature = "bench-instrumentation"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TagApplyMetrics {
    pub source_rows: usize,
    pub selector_attempts: usize,
    pub cell_comparisons: usize,
    pub source_rows_copied: usize,
    pub inserted_rows: usize,
    pub replacement_rows: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagApplyError {
    InvalidPlan(PlanError),
}

impl From<PlanError> for TagApplyError {
    fn from(value: PlanError) -> Self {
        Self::InvalidPlan(value)
    }
}

impl EventEditPlan {
    pub fn apply_tags<R: AsRef<[String]>>(
        &self,
        source: &[R],
    ) -> Result<TagEditOutcome, TagApplyError> {
        apply_tag_edit(self.tag_edit()?, source)
    }

    pub fn apply_partitioned_tags<P: AsRef<[String]>, Q: AsRef<[String]>>(
        &self,
        public: &[P],
        private: &[Q],
    ) -> Result<PartitionedTagEditOutcome, TagApplyError> {
        apply_partitioned_tag_edit(self.partitioned_edit()?, public, private)
    }
}

impl TagItemSelector {
    /// Whether this capability-owned logical item exists in the raw tag
    /// document. Consumers use the same selector for observation and edit
    /// construction, so legacy/current identity cannot drift between them.
    pub fn matches_any<R: AsRef<[String]>>(&self, source: &[R]) -> bool {
        #[cfg(any(test, feature = "bench-instrumentation"))]
        let mut metrics = TagApplyMetrics::default();
        find_first(
            self,
            source,
            #[cfg(any(test, feature = "bench-instrumentation"))]
            &mut metrics,
        )
        .is_some()
    }
}

fn apply_partitioned_tag_edit<P: AsRef<[String]>, Q: AsRef<[String]>>(
    edit: &PartitionedTagEdit,
    public: &[P],
    private: &[Q],
) -> Result<PartitionedTagEditOutcome, TagApplyError> {
    edit.validate()?;
    let public = match edit.public() {
        Some(edit) => apply_tag_edit(edit, public)?,
        None => unchanged(public.len()),
    };
    let private = match edit.private() {
        Some(edit) => apply_tag_edit(edit, private)?,
        None => unchanged(private.len()),
    };
    Ok(PartitionedTagEditOutcome { public, private })
}

fn unchanged(_source_rows: usize) -> TagEditOutcome {
    TagEditOutcome {
        replacement: None,
        #[cfg(any(test, feature = "bench-instrumentation"))]
        metrics: TagApplyMetrics {
            source_rows: _source_rows,
            ..TagApplyMetrics::default()
        },
    }
}

fn apply_tag_edit<R: AsRef<[String]>>(
    edit: &TagEdit,
    source: &[R],
) -> Result<TagEditOutcome, TagApplyError> {
    edit.validate()?;
    #[cfg(any(test, feature = "bench-instrumentation"))]
    let mut metrics = TagApplyMetrics {
        source_rows: source.len(),
        ..TagApplyMetrics::default()
    };

    let (removed, inserted, insertion) = match edit {
        TagEdit::EnsurePresent {
            selector,
            rows,
            insertion,
        } => {
            if find_first(
                selector,
                source,
                #[cfg(any(test, feature = "bench-instrumentation"))]
                &mut metrics,
            )
            .is_some()
            {
                return Ok(TagEditOutcome {
                    replacement: None,
                    #[cfg(any(test, feature = "bench-instrumentation"))]
                    metrics,
                });
            }
            let at = resolve_insertion(
                insertion,
                source,
                &[],
                #[cfg(any(test, feature = "bench-instrumentation"))]
                &mut metrics,
            );
            (Vec::new(), rows.as_slice(), at)
        }
        TagEdit::Remove { selector } => {
            let removed = find_all(
                std::slice::from_ref(selector),
                source,
                #[cfg(any(test, feature = "bench-instrumentation"))]
                &mut metrics,
            );
            if removed.is_empty() {
                return Ok(TagEditOutcome {
                    replacement: None,
                    #[cfg(any(test, feature = "bench-instrumentation"))]
                    metrics,
                });
            }
            (removed, &[][..], 0)
        }
        TagEdit::Rewrite {
            selectors,
            rows,
            insertion,
        } => {
            let removed = find_all(
                selectors,
                source,
                #[cfg(any(test, feature = "bench-instrumentation"))]
                &mut metrics,
            );
            let at = resolve_insertion(
                insertion,
                source,
                &removed,
                #[cfg(any(test, feature = "bench-instrumentation"))]
                &mut metrics,
            );
            if removed.is_empty() && rows.is_empty() {
                return Ok(TagEditOutcome {
                    replacement: None,
                    #[cfg(any(test, feature = "bench-instrumentation"))]
                    metrics,
                });
            }
            (removed, rows.as_slice(), at)
        }
    };

    let normalized_insertion = normalize_insertion(insertion, &removed);
    let removed_rows: usize = removed.iter().map(|range| range.end - range.start).sum();
    let mut output = Vec::with_capacity(source.len() - removed_rows + inserted.len());
    let mut range_index = 0;
    let mut source_index = 0;
    let mut did_insert = false;

    while source_index <= source.len() {
        if !did_insert && source_index == normalized_insertion {
            output.extend(inserted.iter().cloned());
            did_insert = true;
            #[cfg(any(test, feature = "bench-instrumentation"))]
            {
                metrics.inserted_rows += inserted.len();
            }
        }
        if source_index == source.len() {
            break;
        }
        if let Some(range) = removed.get(range_index) {
            if source_index == range.start {
                source_index = range.end;
                range_index += 1;
                continue;
            }
        }
        output.push(source[source_index].as_ref().to_vec());
        #[cfg(any(test, feature = "bench-instrumentation"))]
        {
            metrics.source_rows_copied += 1;
        }
        source_index += 1;
    }

    if output.len() == source.len()
        && output
            .iter()
            .zip(source)
            .all(|(output, source)| output.as_slice() == source.as_ref())
    {
        Ok(TagEditOutcome {
            replacement: None,
            #[cfg(any(test, feature = "bench-instrumentation"))]
            metrics,
        })
    } else {
        #[cfg(any(test, feature = "bench-instrumentation"))]
        {
            metrics.replacement_rows = output.len();
        }
        Ok(TagEditOutcome {
            replacement: Some(output),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            metrics,
        })
    }
}

fn normalize_insertion(insertion: usize, removed: &[Range<usize>]) -> usize {
    removed
        .iter()
        .find(|range| range.start < insertion && insertion < range.end)
        .map_or(insertion, |range| range.start)
}

fn resolve_insertion<R: AsRef<[String]>>(
    insertion: &TagInsertion,
    source: &[R],
    matches: &[Range<usize>],
    #[cfg(any(test, feature = "bench-instrumentation"))] metrics: &mut TagApplyMetrics,
) -> usize {
    match insertion {
        TagInsertion::Boundary { boundary } => boundary_index(*boundary, source.len()),
        TagInsertion::FirstMatchOr { fallback } => matches.first().map_or_else(
            || boundary_index(*fallback, source.len()),
            |range| range.start,
        ),
        TagInsertion::BeforeFirst { anchor, fallback } => find_first(
            anchor,
            source,
            #[cfg(any(test, feature = "bench-instrumentation"))]
            metrics,
        )
        .map_or_else(
            || boundary_index(*fallback, source.len()),
            |range| range.start,
        ),
        TagInsertion::AfterLast { anchor, fallback } => find_last(
            anchor,
            source,
            #[cfg(any(test, feature = "bench-instrumentation"))]
            metrics,
        )
        .map_or_else(
            || boundary_index(*fallback, source.len()),
            |range| range.end,
        ),
    }
}

fn boundary_index(boundary: Boundary, len: usize) -> usize {
    match boundary {
        Boundary::Start => 0,
        Boundary::End => len,
    }
}

fn find_all<R: AsRef<[String]>>(
    selectors: &[TagItemSelector],
    source: &[R],
    #[cfg(any(test, feature = "bench-instrumentation"))] metrics: &mut TagApplyMetrics,
) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut at = 0;
    while at < source.len() {
        let len = selectors
            .iter()
            .filter_map(|selector| {
                match_at(
                    selector,
                    source,
                    at,
                    #[cfg(any(test, feature = "bench-instrumentation"))]
                    metrics,
                )
            })
            .max();
        if let Some(len) = len {
            ranges.push(at..at + len);
            at += len;
        } else {
            at += 1;
        }
    }
    ranges
}

fn find_first<R: AsRef<[String]>>(
    selector: &TagItemSelector,
    source: &[R],
    #[cfg(any(test, feature = "bench-instrumentation"))] metrics: &mut TagApplyMetrics,
) -> Option<Range<usize>> {
    (0..source.len()).find_map(|at| {
        match_at(
            selector,
            source,
            at,
            #[cfg(any(test, feature = "bench-instrumentation"))]
            metrics,
        )
        .map(|len| at..at + len)
    })
}

fn find_last<R: AsRef<[String]>>(
    selector: &TagItemSelector,
    source: &[R],
    #[cfg(any(test, feature = "bench-instrumentation"))] metrics: &mut TagApplyMetrics,
) -> Option<Range<usize>> {
    (0..source.len()).rev().find_map(|at| {
        match_at(
            selector,
            source,
            at,
            #[cfg(any(test, feature = "bench-instrumentation"))]
            metrics,
        )
        .map(|len| at..at + len)
    })
}

fn match_at<R: AsRef<[String]>>(
    selector: &TagItemSelector,
    source: &[R],
    at: usize,
    #[cfg(any(test, feature = "bench-instrumentation"))] metrics: &mut TagApplyMetrics,
) -> Option<usize> {
    selector.alternatives().iter().find_map(|pattern| {
        #[cfg(any(test, feature = "bench-instrumentation"))]
        {
            metrics.selector_attempts += 1;
        }
        item_matches(
            pattern,
            source,
            at,
            #[cfg(any(test, feature = "bench-instrumentation"))]
            metrics,
        )
        .then_some(pattern.rows().len())
    })
}

fn item_matches<R: AsRef<[String]>>(
    pattern: &TagItemPattern,
    source: &[R],
    at: usize,
    #[cfg(any(test, feature = "bench-instrumentation"))] metrics: &mut TagApplyMetrics,
) -> bool {
    if source.len().saturating_sub(at) < pattern.rows().len() {
        return false;
    }
    pattern.rows().iter().enumerate().all(|(offset, row)| {
        row_matches(
            row,
            source[at + offset].as_ref(),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            metrics,
        )
    })
}

fn row_matches(
    pattern: &TagRowPattern,
    row: &[String],
    #[cfg(any(test, feature = "bench-instrumentation"))] metrics: &mut TagApplyMetrics,
) -> bool {
    if pattern
        .exact_cell_count()
        .is_some_and(|count| row.len() != count)
        || row.len() < pattern.cells().len()
    {
        return false;
    }
    pattern.cells().iter().enumerate().all(|(index, value)| {
        #[cfg(any(test, feature = "bench-instrumentation"))]
        {
            metrics.cell_comparisons += 1;
        }
        row[index] == *value
    })
}

#[cfg(test)]
#[path = "tags/tests.rs"]
mod tests;
