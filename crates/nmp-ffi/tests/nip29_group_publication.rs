//! #1033 Lane A -- the FFI projection of `nmp::nip29` end to end, through
//! `nmp_ffi::nip29`'s own public objects rather than `#[cfg(test)]` unit
//! tests inside the crate. This is the boundary a Swift/Kotlin app actually
//! links against: `FfiRelayScope`, `FfiGroup`, `FfiGroupPredicate`,
//! `member_list_includes`/`admin_list_includes`, and the untracked
//! `NmpGroupReceiptStream` a group write returns.
//!
//! Socket-level delivery (which relay actually received which bytes) is
//! `crates/nmp/tests/group_publication_door.rs`'s job (owned by a sibling
//! lane) -- this file has no `nmp-test-support` dependency and proves the
//! PROJECTION instead: an app's calls, one FFI hop away from `nmp::nip29`,
//! produce the same shapes and the same typed refusals the direct-Rust door
//! does.

use std::collections::HashMap;
use std::sync::Arc;

use nmp_ffi::convert::FfiError;
use nmp_ffi::facade::{NmpEngine, NmpEngineConfig};
use nmp_ffi::nip29::{
    admin_list_includes, groups_whose_record_matches, member_list_includes, FfiGroupPredicate,
    FfiGroupRecord, FfiRelayScope,
};
use nmp_ffi::types::{
    FfiAccessContext, FfiBinding, FfiEventBuilder, FfiFilter, FfiIdentityField, FfiSourceAuthority,
};

fn host(n: u16) -> String {
    format!("wss://host-{n}.example.com")
}

fn engine() -> Arc<NmpEngine> {
    NmpEngine::new(NmpEngineConfig::default()).expect("an in-memory engine builds")
}

/// #1033's own multi-host falsifier, verified through the FFI objects: a
/// two-host group `read` yields ONE `FfiLiveQuery` with one complete branch
/// per host, each branch pinned to its own host alone -- never
/// `Pinned({A, B})`, never a list the app has to merge.
#[test]
fn a_multi_host_listing_is_one_live_query_with_one_branch_per_host() {
    let scope = FfiRelayScope::on(vec![host(1), host(2)]).expect("two hosts parse");
    let query = scope
        .group("photographers".to_string())
        .read(FfiFilter {
            kinds: Some(vec![9]),
            ..FfiFilter::default()
        })
        .expect("a two-host read declares two branches");

    assert_eq!(query.branches.len(), 2);
    for (branch, expected_host) in query.branches.iter().zip([host(1), host(2)]) {
        assert_eq!(
            branch.source,
            FfiSourceAuthority::Pinned {
                relays: vec![expected_host]
            },
            "each listing branch is pinned to exactly one host"
        );
        assert_eq!(branch.access, FfiAccessContext::Public);
    }
    assert_eq!(query.aggregate_result_limit, None);
}

/// `admin_list_includes`/`member_list_includes` compose through
/// `FfiGroupPredicate::union`/`intersect`/`minus` -- the grammar's own set
/// algebra, never a second combinator vocabulary at the FFI boundary.
#[test]
fn predicates_compose_through_union_intersect_and_minus() {
    let scope = FfiRelayScope::on(vec![host(1)]).expect("one host parses");
    let me = || FfiBinding::Reactive {
        field: FfiIdentityField::ActivePubkey,
    };
    let member = member_list_includes(me()).expect("reactive subjects are always valid");
    let admin = admin_list_includes(me()).expect("reactive subjects are always valid");

    let engine = engine();
    for ids in [
        member.clone().union(vec![admin.clone()]),
        member.clone().intersect(vec![admin.clone()]),
        member.minus(vec![admin]),
    ] {
        let watching = scope
            .observe_records(
                engine.clone(),
                FfiGroupPredicate::naming(ids),
                vec![FfiGroupRecord::Metadata],
                None,
            )
            .expect("a composed predicate still opens over every host");
        watching.cancel();
    }
    engine.shutdown();
}

/// The #1252 capability at the boundary: "every group this relay hosts" is a
/// predicate an app can phrase, and it needs no id set of its own. A boundary
/// that could only phrase a membership question or a known-id list would
/// leave a directory screen hand-building its own demand and hand-parsing
/// kind:39000 rows, which is the state #1246 otherwise ended.
#[test]
fn an_unconstrained_directory_is_phrasable_at_the_boundary() {
    let scope = FfiRelayScope::on(vec![host(1), host(2)]).expect("two hosts parse");
    let engine = engine();
    let watching = scope
        .observe_records(
            engine.clone(),
            FfiGroupPredicate::all(),
            vec![FfiGroupRecord::Metadata],
            Some(250),
        )
        .expect("a two-host directory opens");
    watching.cancel();
    engine.shutdown();
}

/// The refusal the general spelling carries survives to the boundary: a
/// selection naming a kind the group's host is not authoritative for is a
/// typed error, not a read that silently under-resolves.
#[test]
fn a_selection_naming_a_foreign_kind_is_refused_at_the_boundary() {
    let refusal = groups_whose_record_matches(FfiFilter {
        kinds: Some(vec![10009]),
        ..FfiFilter::default()
    })
    .expect_err("kind:10009 is not a relay-signed group record");
    assert_eq!(
        refusal,
        FfiError::GroupIdSelectionNotAGroupRecordKind { kind: 10009 }
    );
    let refusal = groups_whose_record_matches(FfiFilter::default())
        .expect_err("a selection naming no kind is refused");
    assert_eq!(refusal, FfiError::GroupIdSelectionNamesNoKind);
}

/// A group is an identity, not a subscription: forming a `FfiRelayScope`
/// and narrowing to a group contacts nothing and needs no engine at all.
/// `FfiGroup::read` mints the same one-branch-per-host `#h`-scoped live
/// query the direct-Rust `Group::read` does.
#[test]
fn a_group_read_is_one_branch_per_host_scoped_by_h() {
    let scope = FfiRelayScope::on(vec![host(1), host(2)]).expect("two hosts parse");
    let group = scope.group("photographers".to_string());

    let query = group
        .read(FfiFilter::default())
        .expect("a plain selection scopes");
    assert_eq!(query.branches.len(), 2);
    for (branch, expected_host) in query.branches.iter().zip([host(1), host(2)]) {
        assert_eq!(
            branch.source,
            FfiSourceAuthority::Pinned {
                relays: vec![expected_host]
            }
        );
        assert_eq!(
            branch.selection.tags.get("h"),
            Some(&FfiBinding::Literal {
                values: vec!["photographers".to_string()]
            })
        );
    }

    // A read selection naming its own `#h` is refused before any query
    // forms: the retained group id is the sole semantic source of that row.
    let mut tags = HashMap::new();
    tags.insert(
        "h".to_string(),
        FfiBinding::Literal {
            values: vec!["elsewhere".to_string()],
        },
    );
    match group.read(FfiFilter {
        tags,
        ..FfiFilter::default()
    }) {
        Err(FfiError::GroupCallerSuppliedContextConstraint) => {}
        other => panic!("expected GroupCallerSuppliedContextConstraint, got {other:?}"),
    }
}

/// Every named group operation -- and the generic `publish` door -- reach
/// the ONE publish door headless: no relay in the scope needs to be
/// reachable for the write to be ACCEPTED at the engine's door.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_group_operation_reaches_the_one_publish_door() {
    let engine = engine();
    let author = nostr::Keys::generate().public_key().to_hex();
    let subject = nostr::Keys::generate().public_key().to_hex();
    let scope = FfiRelayScope::on(vec![host(1), host(2)]).expect("two hosts parse");
    let group = scope.group("photographers".to_string());

    let stream = group
        .publish(
            engine.clone(),
            author.clone(),
            FfiEventBuilder {
                kind: 9,
                tags: vec![],
                content: "first light".to_string(),
                created_at: None,
            },
        )
        .expect("an ordinary group publish reaches the door");
    // No receipt id exists on this stream -- draining one status is enough
    // to prove the write was accepted into the engine's write plane.
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("a status must arrive within 5s")
        .expect("next() is not a misuse");
    assert!(status.is_some(), "the untracked stream delivers a status");

    for (name, outcome) in [
        (
            "join_request",
            group.join_request(engine.clone(), author.clone(), Some("code".to_string())),
        ),
        (
            "leave_request",
            group.leave_request(engine.clone(), author.clone()),
        ),
        (
            "add_user",
            group.add_user(engine.clone(), author.clone(), subject.clone(), None),
        ),
        (
            "remove_user",
            group.remove_user(engine.clone(), author.clone(), subject.clone()),
        ),
        (
            "edit_metadata",
            group.edit_metadata(
                engine.clone(),
                author.clone(),
                nmp_ffi::nip29::FfiGroupMetadataEdit {
                    name: Some("Photographers".to_string()),
                    ..nmp_ffi::nip29::FfiGroupMetadataEdit::default()
                },
            ),
        ),
        (
            "delete_event",
            group.delete_event(engine.clone(), author.clone(), "09".repeat(32)),
        ),
        (
            "create_group",
            group.create_group(engine.clone(), author.clone()),
        ),
        (
            "delete_group",
            group.delete_group(engine.clone(), author.clone()),
        ),
        (
            "create_invite",
            group.create_invite(engine.clone(), author.clone(), "code".to_string()),
        ),
    ] {
        assert!(
            outcome.is_ok(),
            "{name} must reach the one publish door like every other group write"
        );
    }
}

/// A caller-supplied `h` tag never reaches the door: the refusal is
/// synchronous and typed, before any receipt stream exists.
#[test]
fn a_caller_supplied_context_is_refused_before_any_receipt_stream_exists() {
    let engine = engine();
    let author = nostr::Keys::generate().public_key().to_hex();
    let scope = FfiRelayScope::on(vec![host(1)]).expect("one host parses");
    let group = scope.group("photographers".to_string());

    let refused = group.publish(
        engine,
        author,
        FfiEventBuilder {
            kind: 9,
            tags: vec![vec!["h".to_string(), "photographers".to_string()]],
            content: String::new(),
            created_at: None,
        },
    );
    match refused {
        Err(FfiError::GroupCallerSuppliedContext) => {}
        Err(other) => panic!("expected GroupCallerSuppliedContext, got {other:?}"),
        Ok(_) => panic!("expected GroupCallerSuppliedContext, got Ok"),
    }
}

/// `FfiRelayScope::on` restores fallibility at the FFI boundary the same
/// way every other caller-suppliable relay input in this crate does: an
/// empty set or a malformed host is a typed refusal, never a panic.
#[test]
fn relay_scope_construction_is_a_typed_refusal_not_a_panic() {
    match FfiRelayScope::on(vec![]) {
        Err(FfiError::EmptyRelayScope) => {}
        other => panic!("expected EmptyRelayScope, got {other:?}"),
    }
    match FfiRelayScope::on(vec!["not-a-url".to_string()]) {
        Err(FfiError::InvalidRelayUrl { got }) => assert_eq!(got, "not-a-url"),
        other => panic!("expected InvalidRelayUrl, got {other:?}"),
    }
}
