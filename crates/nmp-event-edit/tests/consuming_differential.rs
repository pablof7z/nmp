use nmp_event_edit::{
    Boundary, EventEditPlan, TagEdit, TagInsertion, TagItemPattern, TagItemSelector, TagRowPattern,
};
use proptest::prelude::*;

struct Cell(String);

impl AsRef<str> for Cell {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

fn selector(target: &str) -> TagItemSelector {
    TagItemSelector::one(
        TagItemPattern::new(vec![TagRowPattern::prefix(vec![
            "item".to_owned(),
            target.to_owned(),
        ])
        .unwrap()])
        .unwrap(),
    )
}

proptest! {
    #[test]
    fn consuming_and_borrowed_paths_have_the_same_structural_meaning(
        target in "[a-z]{1,12}",
        raw in prop::collection::vec((0_u8..4, "[a-z]{0,12}", "[a-z]{0,12}"), 0..256),
    ) {
        let source = raw
            .into_iter()
            .map(|(shape, identity, extra)| match shape {
                0 => vec!["item".to_owned(), target.clone(), extra],
                1 => vec!["item".to_owned(), identity, extra],
                2 => vec!["unknown".to_owned(), identity, extra],
                _ => vec!["item".to_owned()],
            })
            .collect::<Vec<_>>();
        let selected = selector(&target);
        let plans = [
            EventEditPlan::tags(TagEdit::remove(selected.clone())),
            EventEditPlan::tags(
                TagEdit::ensure_present(
                    selected.clone(),
                    vec![vec!["item".to_owned(), target.clone(), "current".to_owned()]],
                    TagInsertion::end(),
                )
                .unwrap(),
            ),
            EventEditPlan::tags(
                TagEdit::rewrite(
                    vec![selected],
                    vec![vec!["item".to_owned(), target.clone(), "current".to_owned()]],
                    TagInsertion::first_match_or(Boundary::End),
                )
                .unwrap(),
            ),
        ];

        for plan in plans {
            let expected = plan
                .apply_tags(&source)
                .unwrap()
                .replacement
                .unwrap_or_else(|| source.clone());
            let transient = source
                .iter()
                .map(|row| row.iter().cloned().map(Cell).collect())
                .collect();
            let actual = plan
                .apply_tags_consuming(transient, |literal| Cell(literal.to_owned()))
                .unwrap();
            let actual = actual
                .rows
                .iter()
                .map(|row| row.iter().map(AsRef::as_ref).collect::<Vec<_>>())
                .collect::<Vec<_>>();
            let expected = expected
                .iter()
                .map(|row| row.iter().map(String::as_str).collect::<Vec<_>>())
                .collect::<Vec<_>>();
            prop_assert_eq!(actual, expected);
        }
    }
}
