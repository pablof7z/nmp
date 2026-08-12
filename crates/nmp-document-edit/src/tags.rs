use std::ops::Range;

use crate::{
    Boundary, DocumentEditPlan, PartitionedTagEdit, PlanError, TagEdit, TagInsertion,
    TagItemPattern, TagItemSelector, TagRowPattern,
};

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

impl DocumentEditPlan {
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
mod tests {
    use super::*;
    use crate::{Partition, PartitionedTagEdit};

    fn row(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn selector(patterns: Vec<Vec<Vec<&str>>>) -> TagItemSelector {
        TagItemSelector::new(
            patterns
                .into_iter()
                .map(|rows| {
                    TagItemPattern::new(
                        rows.into_iter()
                            .map(|prefix| TagRowPattern::prefix(row(&prefix)).unwrap())
                            .collect(),
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn two_row_item_is_removed_as_one_without_splitting_neighbors() {
        let source = vec![
            row(&["x", "before"]),
            row(&["a", "identity", "extra"]),
            row(&["e", "proof", "legacy", "cells"]),
            row(&["x", "after"]),
        ];
        let edit = TagEdit::remove(selector(vec![vec![
            vec!["a", "identity"],
            vec!["e", "proof"],
        ]]));
        let outcome = apply_tag_edit(&edit, &source).unwrap();
        assert_eq!(
            outcome.replacement,
            Some(vec![row(&["x", "before"]), row(&["x", "after"])])
        );
        assert_eq!(outcome.metrics.source_rows, 4);
        assert_eq!(outcome.metrics.source_rows_copied, 2);
    }

    #[test]
    fn unrelated_unknown_malformed_duplicate_and_extra_cells_are_exact() {
        let target = "target";
        let source = vec![
            row(&[]),
            row(&["unknown", "01", "1e0"]),
            row(&["p", target, "hint", "pet", "extra"]),
            row(&["p", "other", "hint"]),
            row(&["p", target, "second"]),
            row(&["unknown", "01", "1e0"]),
        ];
        let edit = TagEdit::remove(selector(vec![vec![vec!["p", target]]]));
        let outcome = apply_tag_edit(&edit, &source).unwrap();
        assert_eq!(
            outcome.replacement,
            Some(vec![
                row(&[]),
                row(&["unknown", "01", "1e0"]),
                row(&["p", "other", "hint"]),
                row(&["unknown", "01", "1e0"]),
            ])
        );
    }

    #[test]
    fn unchanged_partition_is_borrowed_logically_and_never_rebuilt() {
        let public = vec![row(&["x", "public"]), row(&["p", "target"])];
        let private = vec![row(&["secret", "keep", "order"]), row(&[])];
        let edit = PartitionedTagEdit::only(
            Partition::Public,
            TagEdit::remove(selector(vec![vec![vec!["p", "target"]]])),
        );
        let plan = DocumentEditPlan::partitioned_tags(edit).unwrap();
        let outcome = plan.apply_partitioned_tags(&public, &private).unwrap();
        assert_eq!(
            outcome.public.replacement,
            Some(vec![row(&["x", "public"])])
        );
        assert_eq!(outcome.private.replacement, None);
        assert_eq!(outcome.private.metrics.source_rows_copied, 0);
    }

    #[test]
    fn public_and_private_partitions_edit_and_preserve_order_independently() {
        let public = vec![
            row(&["keep", "public-a"]),
            row(&["drop", "public"]),
            row(&["keep", "public-b"]),
        ];
        let private = vec![
            row(&["keep", "private-a"]),
            row(&["drop", "private"]),
            row(&["keep", "private-b"]),
            row(&["drop", "private"]),
        ];
        let edit = PartitionedTagEdit::new(
            Some(TagEdit::remove(selector(vec![vec![vec![
                "drop", "public",
            ]]]))),
            Some(
                TagEdit::rewrite(
                    vec![selector(vec![vec![vec!["drop", "private"]]])],
                    vec![row(&["current", "private"])],
                    TagInsertion::first_match_or(Boundary::End),
                )
                .unwrap(),
            ),
        )
        .unwrap();
        let outcome = DocumentEditPlan::partitioned_tags(edit)
            .unwrap()
            .apply_partitioned_tags(&public, &private)
            .unwrap();

        assert_eq!(
            outcome.public.replacement,
            Some(vec![row(&["keep", "public-a"]), row(&["keep", "public-b"]),])
        );
        assert_eq!(
            outcome.private.replacement,
            Some(vec![
                row(&["keep", "private-a"]),
                row(&["current", "private"]),
                row(&["keep", "private-b"]),
            ])
        );
    }

    #[test]
    fn exact_rows_and_anchored_insertions_have_executable_meaning() {
        let exact = TagItemSelector::one(
            TagItemPattern::new(vec![TagRowPattern::exact(row(&["x", "id"]), 2).unwrap()]).unwrap(),
        );
        let anchor = selector(vec![vec![vec!["anchor"]]]);
        let source = vec![
            row(&["x", "id", "legacy-extra"]),
            row(&["anchor"]),
            row(&["tail"]),
        ];

        let before = TagEdit::ensure_present(
            exact.clone(),
            vec![row(&["x", "id"])],
            TagInsertion::before_first(anchor.clone(), Boundary::End),
        )
        .unwrap();
        assert_eq!(
            DocumentEditPlan::tags(before)
                .apply_tags(&source)
                .unwrap()
                .replacement,
            Some(vec![
                row(&["x", "id", "legacy-extra"]),
                row(&["x", "id"]),
                row(&["anchor"]),
                row(&["tail"]),
            ])
        );

        let after = TagEdit::rewrite(
            vec![exact],
            vec![row(&["replacement"])],
            TagInsertion::after_last(anchor, Boundary::Start),
        )
        .unwrap();
        assert_eq!(
            DocumentEditPlan::tags(after)
                .apply_tags(&[row(&["x", "id"]), row(&["anchor"]), row(&["tail"])])
                .unwrap()
                .replacement,
            Some(vec![
                row(&["anchor"]),
                row(&["replacement"]),
                row(&["tail"]),
            ])
        );

        let at_start = TagEdit::ensure_present(
            selector(vec![vec![vec!["new"]]]),
            vec![row(&["new"])],
            TagInsertion::start(),
        )
        .unwrap();
        assert_eq!(
            DocumentEditPlan::tags(at_start)
                .apply_tags(&[row(&["tail"])])
                .unwrap()
                .replacement,
            Some(vec![row(&["new"]), row(&["tail"])])
        );
    }

    #[test]
    fn plan_round_trips_without_a_runtime_codec() {
        let plan = DocumentEditPlan::tags(
            TagEdit::ensure_present(
                selector(vec![vec![vec!["x", "id"]], vec![vec!["legacy", "id"]]]),
                vec![row(&["x", "id", "current"])],
                TagInsertion::end(),
            )
            .unwrap(),
        );
        let encoded = serde_json::to_vec(&plan).unwrap();
        let decoded: DocumentEditPlan = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, plan);
    }

    #[test]
    fn large_single_edit_reconstructs_once_and_copies_each_retained_row_once() {
        let mut source = (0..10_000)
            .map(|index| row(&["unknown", &index.to_string()]))
            .collect::<Vec<_>>();
        source.insert(7_000, row(&["x", "target"]));
        let edit = TagEdit::remove(selector(vec![vec![vec!["x", "target"]]]));
        let outcome = apply_tag_edit(&edit, &source).unwrap();
        assert_eq!(outcome.metrics.source_rows, 10_001);
        assert_eq!(outcome.metrics.source_rows_copied, 10_000);
        assert_eq!(outcome.replacement.unwrap().len(), 10_000);
    }

    #[test]
    fn identical_in_place_rewrite_is_not_a_replacement() {
        let source = vec![row(&["x", "target", "exact"]), row(&["keep", "01"])];
        let target = selector(vec![vec![vec!["x", "target"]]]);
        let edit = TagEdit::rewrite(
            vec![target],
            vec![row(&["x", "target", "exact"])],
            TagInsertion::first_match_or(Boundary::End),
        )
        .unwrap();
        let outcome = apply_tag_edit(&edit, &source).unwrap();
        assert_eq!(outcome.replacement, None);
        assert_eq!(outcome.metrics.source_rows_copied, 1);
        assert_eq!(outcome.metrics.inserted_rows, 1);
        assert_eq!(outcome.metrics.replacement_rows, 0);
    }
}
