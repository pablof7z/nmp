use super::*;
use crate::{JsonFieldEdit, JsonMissing, Occurrences};

#[test]
fn unrelated_json_bytes_survive_a_value_edit_exactly() {
    let source =
        r#"{ "dupe\u0064": 1e+09, "target" : false, "duped":-0.0, "nested":{"z":2,"z":3} }"#;
    let plan = EventEditPlan::json_object(
        JsonFieldEdit::set("target", "true", Occurrences::All, JsonMissing::Insert).unwrap(),
    );
    let outcome = plan.apply_json_object(source).unwrap();
    assert_eq!(
        outcome.replacement.as_deref(),
        Some(r#"{ "dupe\u0064": 1e+09, "target" : true, "duped":-0.0, "nested":{"z":2,"z":3} }"#)
    );
    assert_eq!(outcome.patches.len(), 1);
    assert_eq!(outcome.metrics.source_bytes, source.len());
}

#[test]
fn duplicate_decoded_keys_obey_the_explicit_occurrence_policy() {
    let source = r#"{"x":1,"\u0078":2,"x":3}"#;
    let plan = EventEditPlan::json_object(
        JsonFieldEdit::set("x", "9", Occurrences::Last, JsonMissing::NoChange).unwrap(),
    );

    let first = EventEditPlan::json_object(
        JsonFieldEdit::set("x", "7", Occurrences::First, JsonMissing::NoChange).unwrap(),
    );
    assert_eq!(
        first
            .apply_json_object(source)
            .unwrap()
            .replacement
            .as_deref(),
        Some(r#"{"x":7,"\u0078":2,"x":3}"#)
    );
    assert_eq!(
        plan.apply_json_object(source)
            .unwrap()
            .replacement
            .as_deref(),
        Some(r#"{"x":1,"\u0078":2,"x":9}"#)
    );
}

#[test]
fn removing_disjoint_duplicate_runs_keeps_unselected_members_byte_exact() {
    let source = r#"{ "x":1 , "keep":01, "x":2, "x":3 , "tail":1e0 }"#;
    // The source intentionally contains JSON-invalid `01`; use valid but
    // lexically distinctive values instead.
    let source = source.replace(":01", ":0.10e1");
    let plan = EventEditPlan::json_object(JsonFieldEdit::remove("x", Occurrences::All).unwrap());
    assert_eq!(
        plan.apply_json_object(&source)
            .unwrap()
            .replacement
            .as_deref(),
        Some(r#"{  "keep":0.10e1,  "tail":1e0 }"#)
    );
}

#[test]
fn missing_field_insertion_does_not_reserialize_the_object() {
    let source = r#"{ "a" : 1e2, "a": 3  }"#;
    let plan = EventEditPlan::json_object(
        JsonFieldEdit::set("new", "[1, 2]", Occurrences::All, JsonMissing::Insert).unwrap(),
    );
    assert_eq!(
        plan.apply_json_object(source)
            .unwrap()
            .replacement
            .as_deref(),
        Some(r#"{ "a" : 1e2, "a": 3  ,"new":[1, 2]}"#)
    );
}

#[test]
fn no_match_and_no_insert_returns_no_rebuild() {
    let plan = EventEditPlan::json_object(
        JsonFieldEdit::set("missing", "0", Occurrences::All, JsonMissing::NoChange).unwrap(),
    );
    let outcome = plan.apply_json_object(r#"{"x":1}"#).unwrap();
    assert_eq!(outcome.replacement, None);
    assert!(outcome.patches.is_empty());
    assert_eq!(outcome.metrics.replacement_bytes, 0);
}

#[test]
fn selected_value_already_has_exact_bytes_so_no_replacement_is_emitted() {
    let source = r#"{ "target" : 1e+09, "keep":-0.0 }"#;
    let plan = EventEditPlan::json_object(
        JsonFieldEdit::set("target", "1e+09", Occurrences::All, JsonMissing::NoChange).unwrap(),
    );
    let outcome = plan.apply_json_object(source).unwrap();
    assert_eq!(outcome.replacement, None);
    assert!(outcome.patches.is_empty());
    assert_eq!(outcome.metrics.emitted_patches, 0);
    assert_eq!(outcome.metrics.replacement_bytes, 0);
    assert_eq!(outcome.metrics.source_bytes_copied, 0);
}

#[test]
fn large_json_no_op_scans_borrowed_keys_without_building_a_candidate() {
    let mut source = String::from(r#"{"target":"exact""#);
    for index in 0..20_000 {
        source.push_str(&format!(r#", "key-{index}":"{}""#, "v".repeat(64)));
    }
    source.push('}');
    let plan = EventEditPlan::json_object(
        JsonFieldEdit::set(
            "target",
            r#""exact""#,
            Occurrences::All,
            JsonMissing::NoChange,
        )
        .unwrap(),
    );

    let outcome = plan.apply_json_object(&source).unwrap();
    assert_eq!(outcome.replacement, None);
    assert!(outcome.patches.is_empty());
    assert_eq!(outcome.metrics.object_members, 20_001);
    assert_eq!(outcome.metrics.escaped_keys_decoded, 0);
    assert_eq!(outcome.metrics.source_bytes_copied, 0);
}
