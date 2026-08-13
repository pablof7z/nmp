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
    let plan = EventEditPlan::partitioned_tags(edit).unwrap();
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
    let outcome = EventEditPlan::partitioned_tags(edit)
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
        EventEditPlan::tags(before)
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
        EventEditPlan::tags(after)
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
        EventEditPlan::tags(at_start)
            .apply_tags(&[row(&["tail"])])
            .unwrap()
            .replacement,
        Some(vec![row(&["new"]), row(&["tail"])])
    );
}

#[test]
fn plan_round_trips_without_a_runtime_codec() {
    let plan = EventEditPlan::tags(
        TagEdit::ensure_present(
            selector(vec![vec![vec!["x", "id"]], vec![vec!["legacy", "id"]]]),
            vec![row(&["x", "id", "current"])],
            TagInsertion::end(),
        )
        .unwrap(),
    );
    let encoded = serde_json::to_vec(&plan).unwrap();
    let decoded: EventEditPlan = serde_json::from_slice(&encoded).unwrap();
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
