use nmp_event_edit::{
    EventEditPlan, TagEdit, TagInsertion, TagItemPattern, TagItemSelector, TagRowPattern,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct EventFixture {
    kind: u16,
    d: Option<&'static str>,
    tags: Vec<Vec<String>>,
    content: &'static str,
}

fn row(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn plan() -> EventEditPlan {
    let selector = TagItemSelector::one(
        TagItemPattern::new(vec![
            TagRowPattern::prefix(row(&["item", "wanted"])).unwrap()
        ])
        .unwrap(),
    );
    EventEditPlan::tags(
        TagEdit::ensure_present(
            selector,
            vec![row(&["item", "wanted", "current"])],
            TagInsertion::end(),
        )
        .unwrap(),
    )
}

fn apply(plan: &EventEditPlan, source: &EventFixture) -> EventFixture {
    let replacement = plan.apply_tags(&source.tags).unwrap().replacement;
    EventFixture {
        kind: source.kind,
        d: source.d,
        tags: replacement.unwrap_or_else(|| source.tags.clone()),
        content: source.content,
    }
}

#[test]
fn one_plan_applies_to_ordinary_replaceable_and_addressable_bodies() {
    let fixtures = [
        EventFixture {
            kind: 10_001,
            d: None,
            tags: vec![row(&["unknown", "ordinary", "keep"])],
            content: "ordinary content",
        },
        EventFixture {
            kind: 30_001,
            d: Some("address"),
            tags: vec![
                row(&["d", "address"]),
                row(&["unknown", "addressable", "keep"]),
            ],
            content: "addressable content",
        },
    ];

    for fixture in fixtures {
        let edited = apply(&plan(), &fixture);
        let mut expected = fixture.tags.clone();
        expected.push(row(&["item", "wanted", "current"]));
        assert_eq!(edited.tags, expected);
        assert_eq!(edited.kind, fixture.kind);
        assert_eq!(edited.d, fixture.d);
        assert_eq!(edited.content, fixture.content);
    }
}

#[test]
fn apply_accepts_one_capability_normalized_operation_not_history() {
    let plan = plan();
    let encoded = serde_json::to_value(plan).unwrap();
    let edit = encoded.get("edit").expect("versioned edit payload");
    assert!(edit.get("operations").is_none());
    assert!(edit.get("receipts").is_none());
    assert!(edit.get("history").is_none());
}
