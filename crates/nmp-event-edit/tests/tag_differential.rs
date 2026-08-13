use nmp_event_edit::{
    EventEditPlan, TagEdit, TagInsertion, TagItemPattern, TagItemSelector, TagRowPattern,
};
use proptest::prelude::*;

fn selector(target: &str) -> TagItemSelector {
    TagItemSelector::one(
        TagItemPattern::new(vec![TagRowPattern::prefix(vec![
            "item".to_string(),
            target.to_string(),
        ])
        .unwrap()])
        .unwrap(),
    )
}

fn materialize(source: &[Vec<String>], replacement: Option<Vec<Vec<String>>>) -> Vec<Vec<String>> {
    replacement.unwrap_or_else(|| source.to_vec())
}

proptest! {
    #[test]
    fn compiled_single_item_edits_match_an_independent_reference(
        target in "[a-z]{1,12}",
        raw in prop::collection::vec((0_u8..4, "[a-z]{0,12}", "[a-z]{0,12}"), 0..256),
    ) {
        let source = raw
            .into_iter()
            .map(|(shape, identity, extra)| match shape {
                0 => vec!["item".to_string(), target.clone(), extra],
                1 => vec!["item".to_string(), identity, extra],
                2 => vec!["unknown".to_string(), identity, extra],
                _ => vec!["item".to_string()],
            })
            .collect::<Vec<_>>();

        let remove = EventEditPlan::tags(TagEdit::remove(selector(&target)));
        let removed = materialize(&source, remove.apply_tags(&source).unwrap().replacement);
        let reference_removed = source
            .iter()
            .filter(|row| {
                !(row.first().map(String::as_str) == Some("item")
                    && row.get(1).map(String::as_str) == Some(target.as_str()))
            })
            .cloned()
            .collect::<Vec<_>>();
        prop_assert_eq!(removed, reference_removed);

        let ensure = EventEditPlan::tags(
            TagEdit::ensure_present(
                selector(&target),
                vec![vec!["item".to_string(), target.clone()]],
                TagInsertion::end(),
            )
            .unwrap(),
        );
        let ensured = materialize(&source, ensure.apply_tags(&source).unwrap().replacement);
        let mut reference_ensured = source.clone();
        let present = source.iter().any(|row| {
            row.first().map(String::as_str) == Some("item")
                && row.get(1).map(String::as_str) == Some(target.as_str())
        });
        if !present {
            reference_ensured.push(vec!["item".to_string(), target]);
        }
        prop_assert_eq!(ensured, reference_ensured);
    }
}
