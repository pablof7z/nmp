use std::{
    cell::Cell as Counter,
    panic::{catch_unwind, AssertUnwindSafe},
    rc::Rc,
};

use nmp_event_edit::{
    EventEditPlan, Partition, PartitionedTagEdit, TagEdit, TagInsertion, TagItemPattern,
    TagItemSelector, TagRowPattern,
};
use static_assertions::assert_not_impl_any;

struct TransientCell {
    bytes: Vec<u8>,
    identity: usize,
    drops: Rc<Counter<usize>>,
}

assert_not_impl_any!(TransientCell: Clone, std::fmt::Debug, std::fmt::Display, serde::Serialize);
assert_not_impl_any!(Vec<TransientCell>: Clone, std::fmt::Debug, std::fmt::Display, serde::Serialize);

impl AsRef<str> for TransientCell {
    fn as_ref(&self) -> &str {
        std::str::from_utf8(&self.bytes).expect("test cells start as UTF-8")
    }
}

impl Drop for TransientCell {
    fn drop(&mut self) {
        self.bytes.fill(0);
        self.drops.set(self.drops.get() + 1);
    }
}

fn cell(text: &str, identity: usize, drops: &Rc<Counter<usize>>) -> TransientCell {
    TransientCell {
        bytes: text.as_bytes().to_vec(),
        identity,
        drops: Rc::clone(drops),
    }
}

fn row(values: &[&str], first_identity: usize, drops: &Rc<Counter<usize>>) -> Vec<TransientCell> {
    values
        .iter()
        .enumerate()
        .map(|(offset, value)| cell(value, first_identity + offset, drops))
        .collect()
}

fn selector(prefix: &[&str]) -> TagItemSelector {
    TagItemSelector::one(
        TagItemPattern::new(vec![TagRowPattern::prefix(
            prefix.iter().map(|value| (*value).to_owned()).collect(),
        )
        .unwrap()])
        .unwrap(),
    )
}

fn text(rows: &[Vec<TransientCell>]) -> Vec<Vec<&str>> {
    rows.iter()
        .map(|row| row.iter().map(AsRef::as_ref).collect())
        .collect()
}

#[test]
fn unchanged_cells_move_and_only_plan_literals_construct_new_cells() {
    let drops = Rc::new(Counter::new(0));
    let source = vec![
        row(&["keep", "before"], 10, &drops),
        row(&["item", "old"], 20, &drops),
        row(&["keep", "after"], 30, &drops),
    ];
    let plan = EventEditPlan::tags(
        TagEdit::rewrite(
            vec![selector(&["item", "old"])],
            vec![vec!["item".to_owned(), "new".to_owned()]],
            TagInsertion::first_match_or(nmp_event_edit::Boundary::End),
        )
        .unwrap(),
    );
    let constructions = Counter::new(0);
    let outcome = plan
        .apply_tags_consuming(source, |literal| {
            let identity = 100 + constructions.get();
            constructions.set(constructions.get() + 1);
            cell(literal, identity, &drops)
        })
        .unwrap();

    assert!(outcome.changed);
    assert_eq!(
        text(&outcome.rows),
        vec![
            vec!["keep", "before"],
            vec!["item", "new"],
            vec!["keep", "after"]
        ]
    );
    assert_eq!(outcome.rows[0][0].identity, 10);
    assert_eq!(outcome.rows[0][1].identity, 11);
    assert_eq!(outcome.rows[1][0].identity, 100);
    assert_eq!(outcome.rows[1][1].identity, 101);
    assert_eq!(outcome.rows[2][0].identity, 30);
    assert_eq!(outcome.rows[2][1].identity, 31);
    assert_eq!(constructions.get(), 2);
    assert_eq!(drops.get(), 2, "only the removed source row has dropped");

    drop(outcome);
    assert_eq!(drops.get(), 8);
}

#[test]
fn equivalent_rewrite_returns_the_original_owners_without_literal_construction() {
    let drops = Rc::new(Counter::new(0));
    let source = vec![
        row(&["item", "same"], 10, &drops),
        row(&["keep"], 20, &drops),
    ];
    let plan = EventEditPlan::tags(
        TagEdit::rewrite(
            vec![selector(&["item", "same"])],
            vec![vec!["item".to_owned(), "same".to_owned()]],
            TagInsertion::first_match_or(nmp_event_edit::Boundary::End),
        )
        .unwrap(),
    );
    let constructions = Counter::new(0);
    let outcome = plan
        .apply_tags_consuming(source, |literal| {
            constructions.set(constructions.get() + 1);
            cell(literal, 100, &drops)
        })
        .unwrap();

    assert!(!outcome.changed);
    assert_eq!(constructions.get(), 0);
    assert_eq!(outcome.rows[0][0].identity, 10);
    assert_eq!(outcome.rows[0][1].identity, 11);
    assert_eq!(drops.get(), 0);
}

#[test]
fn partitioned_edit_moves_the_unedited_arm_and_edits_the_selected_arm() {
    let public_drops = Rc::new(Counter::new(0));
    let private_drops = Rc::new(Counter::new(0));
    let public = vec![row(&["keep", "public"], 10, &public_drops)];
    let private = vec![
        row(&["item", "old"], 20, &private_drops),
        row(&["keep", "private"], 30, &private_drops),
    ];
    let plan = EventEditPlan::partitioned_tags(PartitionedTagEdit::only(
        Partition::Private,
        TagEdit::rewrite(
            vec![selector(&["item", "old"])],
            vec![vec!["item".to_owned(), "new".to_owned()]],
            TagInsertion::start(),
        )
        .unwrap(),
    ))
    .unwrap();
    let public_constructions = Counter::new(0);
    let private_constructions = Counter::new(0);
    let outcome = plan
        .apply_partitioned_tags_consuming(
            public,
            private,
            |literal| {
                public_constructions.set(public_constructions.get() + 1);
                cell(literal, 100, &public_drops)
            },
            |literal| {
                let identity = 200 + private_constructions.get();
                private_constructions.set(private_constructions.get() + 1);
                cell(literal, identity, &private_drops)
            },
        )
        .unwrap();

    assert!(!outcome.public.changed);
    assert!(outcome.private.changed);
    assert_eq!(outcome.public.rows[0][0].identity, 10);
    assert_eq!(public_constructions.get(), 0);
    assert_eq!(public_drops.get(), 0);
    assert_eq!(private_constructions.get(), 2);
    assert_eq!(private_drops.get(), 2);
    assert_eq!(outcome.private.rows[0][0].identity, 200);
    assert_eq!(outcome.private.rows[0][1].identity, 201);
    assert_eq!(outcome.private.rows[1][0].identity, 30);
}

#[test]
fn invalid_plan_refusal_and_literal_factory_panic_drop_every_owned_cell() {
    let invalid: EventEditPlan = serde_json::from_value(serde_json::json!({
        "version": "v1",
        "edit": {
            "document": "tags",
            "operation": {
                "operation": "rewrite",
                "selectors": [],
                "rows": [],
                "insertion": { "position": "boundary", "boundary": "end" }
            }
        }
    }))
    .unwrap();
    let refusal_drops = Rc::new(Counter::new(0));
    let refused_source = vec![row(&["keep", "owned"], 10, &refusal_drops)];
    assert!(invalid
        .apply_tags_consuming(refused_source, |_| unreachable!())
        .is_err());
    assert_eq!(refusal_drops.get(), 2);

    let panic_drops = Rc::new(Counter::new(0));
    let source = vec![
        row(&["keep", "before"], 10, &panic_drops),
        row(&["item", "old"], 20, &panic_drops),
        row(&["keep", "after"], 30, &panic_drops),
    ];
    let plan = EventEditPlan::tags(
        TagEdit::rewrite(
            vec![selector(&["item", "old"])],
            vec![vec!["item".to_owned(), "new".to_owned()]],
            TagInsertion::first_match_or(nmp_event_edit::Boundary::End),
        )
        .unwrap(),
    );
    let calls = Counter::new(0);
    let panic = catch_unwind(AssertUnwindSafe(|| {
        let _ = plan.apply_tags_consuming(source, |literal| {
            let call = calls.get();
            calls.set(call + 1);
            if call == 1 {
                panic!("second plan literal refused");
            }
            cell(literal, 100 + call, &panic_drops)
        });
    }));

    assert!(panic.is_err());
    assert_eq!(calls.get(), 2);
    assert_eq!(
        panic_drops.get(),
        7,
        "six source cells and one constructed cell drop on unwind"
    );
}

#[test]
fn consuming_mechanism_contains_no_policy_or_crypto_vocabulary() {
    let source = include_str!("../src/tags/consuming.rs").to_ascii_lowercase();
    for forbidden in [
        "nip04",
        "nip44",
        "signer",
        "encrypt",
        "decrypt",
        "cipher",
        "zeroiz",
        "logging",
        "diagnostic",
        "persist",
    ] {
        assert!(
            !source.contains(forbidden),
            "generic consuming mechanism contains forbidden vocabulary: {forbidden}"
        );
    }
}
