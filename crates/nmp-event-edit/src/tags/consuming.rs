use std::ops::Range;

use crate::{
    EventEditPlan, PartitionedTagEdit, TagEdit, TagInsertion, TagItemSelector, TagRowPattern,
};

use super::{boundary_index, normalize_insertion, TagApplyError};

/// Result of applying one tag plan by taking ownership of every source cell.
///
/// `changed == false` means `rows` contains the original owners in their
/// original order. A changed result moves every retained source row and asks
/// the caller to construct cells only for literals retained by the durable
/// plan.
pub struct ConsumingTagEditOutcome<T> {
    pub rows: Vec<Vec<T>>,
    pub changed: bool,
}

pub struct ConsumingPartitionedTagEditOutcome<P, Q> {
    pub public: ConsumingTagEditOutcome<P>,
    pub private: ConsumingTagEditOutcome<Q>,
}

impl EventEditPlan {
    /// Apply a tag edit to move-only caller-owned cells.
    ///
    /// The source is consumed. Retained rows and cells are moved into the
    /// result without cloning. `from_literal` is called only when the plan
    /// inserts a durable literal; the generic mechanism therefore owns no
    /// policy for the caller's cell type or lifecycle.
    pub fn apply_tags_consuming<T, F>(
        &self,
        source: Vec<Vec<T>>,
        from_literal: F,
    ) -> Result<ConsumingTagEditOutcome<T>, TagApplyError>
    where
        T: AsRef<str>,
        F: FnMut(&str) -> T,
    {
        apply_tag_edit(self.tag_edit()?, source, from_literal)
    }

    pub fn apply_partitioned_tags_consuming<P, Q, FP, FQ>(
        &self,
        public: Vec<Vec<P>>,
        private: Vec<Vec<Q>>,
        public_from_literal: FP,
        private_from_literal: FQ,
    ) -> Result<ConsumingPartitionedTagEditOutcome<P, Q>, TagApplyError>
    where
        P: AsRef<str>,
        Q: AsRef<str>,
        FP: FnMut(&str) -> P,
        FQ: FnMut(&str) -> Q,
    {
        apply_partitioned_tag_edit(
            self.partitioned_edit()?,
            public,
            private,
            public_from_literal,
            private_from_literal,
        )
    }
}

fn apply_partitioned_tag_edit<P, Q, FP, FQ>(
    edit: &PartitionedTagEdit,
    public: Vec<Vec<P>>,
    private: Vec<Vec<Q>>,
    public_from_literal: FP,
    private_from_literal: FQ,
) -> Result<ConsumingPartitionedTagEditOutcome<P, Q>, TagApplyError>
where
    P: AsRef<str>,
    Q: AsRef<str>,
    FP: FnMut(&str) -> P,
    FQ: FnMut(&str) -> Q,
{
    edit.validate()?;
    let public = match edit.public() {
        Some(edit) => apply_tag_edit(edit, public, public_from_literal)?,
        None => ConsumingTagEditOutcome {
            rows: public,
            changed: false,
        },
    };
    let private = match edit.private() {
        Some(edit) => apply_tag_edit(edit, private, private_from_literal)?,
        None => ConsumingTagEditOutcome {
            rows: private,
            changed: false,
        },
    };
    Ok(ConsumingPartitionedTagEditOutcome { public, private })
}

fn apply_tag_edit<T, F>(
    edit: &TagEdit,
    mut source: Vec<Vec<T>>,
    mut from_literal: F,
) -> Result<ConsumingTagEditOutcome<T>, TagApplyError>
where
    T: AsRef<str>,
    F: FnMut(&str) -> T,
{
    edit.validate()?;
    let (removed, inserted, insertion) = classify_edit(edit, &source)?;
    if removed.is_empty() && inserted.is_empty() {
        return Ok(ConsumingTagEditOutcome {
            rows: source,
            changed: false,
        });
    }
    if edit_is_equivalent(&source, &removed, inserted, insertion) {
        return Ok(ConsumingTagEditOutcome {
            rows: source,
            changed: false,
        });
    }

    let insertion = normalize_insertion(insertion, &removed);
    let removed_rows: usize = removed.iter().map(|range| range.end - range.start).sum();
    let mut output = Vec::with_capacity(source.len() - removed_rows + inserted.len());
    let mut range_index = 0;
    let mut did_insert = false;
    for (source_index, row) in source.drain(..).enumerate() {
        if !did_insert && source_index == insertion {
            extend_plan_rows(&mut output, inserted, &mut from_literal);
            did_insert = true;
        }
        let removed_here = removed
            .get(range_index)
            .is_some_and(|range| range.contains(&source_index));
        if removed
            .get(range_index)
            .is_some_and(|range| range.end == source_index + 1)
        {
            range_index += 1;
        }
        if !removed_here {
            output.push(row);
        }
    }
    if !did_insert {
        extend_plan_rows(&mut output, inserted, &mut from_literal);
    }
    Ok(ConsumingTagEditOutcome {
        rows: output,
        changed: true,
    })
}

type ClassifiedEdit<'a> = (Vec<Range<usize>>, &'a [Vec<String>], usize);

fn classify_edit<'a, T: AsRef<str>>(
    edit: &'a TagEdit,
    source: &[Vec<T>],
) -> Result<ClassifiedEdit<'a>, TagApplyError> {
    Ok(match edit {
        TagEdit::EnsurePresent {
            selector,
            rows,
            insertion,
        } => {
            if find_first(selector, source).is_some() {
                return Ok((Vec::new(), &[], 0));
            }
            (Vec::new(), rows, resolve_insertion(insertion, source, &[]))
        }
        TagEdit::Remove { selector } => {
            let removed = find_all(std::slice::from_ref(selector), source);
            (removed, &[], 0)
        }
        TagEdit::Rewrite {
            selectors,
            rows,
            insertion,
        } => {
            let removed = find_all(selectors, source);
            let at = resolve_insertion(insertion, source, &removed);
            (removed, rows, at)
        }
    })
}

fn extend_plan_rows<T, F>(output: &mut Vec<Vec<T>>, inserted: &[Vec<String>], from_literal: &mut F)
where
    F: FnMut(&str) -> T,
{
    output.extend(inserted.iter().map(|row| {
        row.iter()
            .map(|literal| from_literal(literal.as_str()))
            .collect()
    }));
}

fn edit_is_equivalent<T: AsRef<str>>(
    source: &[Vec<T>],
    removed: &[Range<usize>],
    inserted: &[Vec<String>],
    insertion: usize,
) -> bool {
    if removed.len() != 1 {
        return false;
    }
    let range = &removed[0];
    normalize_insertion(insertion, removed) == range.start
        && range.end - range.start == inserted.len()
        && source[range.clone()]
            .iter()
            .zip(inserted)
            .all(|(source, inserted)| {
                source.len() == inserted.len()
                    && source
                        .iter()
                        .zip(inserted)
                        .all(|(cell, literal)| cell.as_ref() == literal)
            })
}

fn resolve_insertion<T: AsRef<str>>(
    insertion: &TagInsertion,
    source: &[Vec<T>],
    matches: &[Range<usize>],
) -> usize {
    match insertion {
        TagInsertion::Boundary { boundary } => boundary_index(*boundary, source.len()),
        TagInsertion::FirstMatchOr { fallback } => matches.first().map_or_else(
            || boundary_index(*fallback, source.len()),
            |range| range.start,
        ),
        TagInsertion::BeforeFirst { anchor, fallback } => find_first(anchor, source).map_or_else(
            || boundary_index(*fallback, source.len()),
            |range| range.start,
        ),
        TagInsertion::AfterLast { anchor, fallback } => find_last(anchor, source).map_or_else(
            || boundary_index(*fallback, source.len()),
            |range| range.end,
        ),
    }
}

fn find_all<T: AsRef<str>>(selectors: &[TagItemSelector], source: &[Vec<T>]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut at = 0;
    while at < source.len() {
        let len = selectors
            .iter()
            .filter_map(|selector| match_at(selector, source, at))
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

fn find_first<T: AsRef<str>>(
    selector: &TagItemSelector,
    source: &[Vec<T>],
) -> Option<Range<usize>> {
    (0..source.len()).find_map(|at| match_at(selector, source, at).map(|len| at..at + len))
}

fn find_last<T: AsRef<str>>(selector: &TagItemSelector, source: &[Vec<T>]) -> Option<Range<usize>> {
    (0..source.len())
        .rev()
        .find_map(|at| match_at(selector, source, at).map(|len| at..at + len))
}

fn match_at<T: AsRef<str>>(
    selector: &TagItemSelector,
    source: &[Vec<T>],
    at: usize,
) -> Option<usize> {
    selector.alternatives().iter().find_map(|pattern| {
        if source.len().saturating_sub(at) < pattern.rows().len() {
            return None;
        }
        pattern
            .rows()
            .iter()
            .enumerate()
            .all(|(offset, row)| row_matches(row, &source[at + offset]))
            .then_some(pattern.rows().len())
    })
}

fn row_matches<T: AsRef<str>>(pattern: &TagRowPattern, row: &[T]) -> bool {
    if pattern
        .exact_cell_count()
        .is_some_and(|count| row.len() != count)
        || row.len() < pattern.cells().len()
    {
        return false;
    }
    pattern
        .cells()
        .iter()
        .zip(row)
        .all(|(literal, cell)| literal == cell.as_ref())
}
