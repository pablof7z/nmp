//! Deterministic falsifier for the settled-route process-boundary ordering.
//!
//! A fast local indexer used to let the past-tense publish setup return
//! before its causal EOSE by accident. Delaying query admission makes that
//! ordering explicit: the setup must not return until the receipt contains
//! the settled `Absent` revision that the restart is meant to preserve.

use std::time::Duration;

use nmp::mechanism::outbox::WriteStatus;
use nmp_store::{EventStore, RedbStore};

use super::{NmpWorld, ME};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn past_tense_publish_observes_settled_absence_before_restart() {
    let mut world = NmpWorld::default();
    world.use_durable_store();
    world.log_in_as(ME, &[]);
    world.indexers_finished_without_a_list_for(ME);

    for indexer in world.indexer_names.clone() {
        world.relay_config_mut(&indexer).query_delay = Some(Duration::from_millis(200));
    }

    world
        .publish_note_after_settled_own_absence("into the delayed void")
        .await;

    let me = world.my_pubkey_hex();
    let wanted = format!("author routes are Absent for {me}");
    assert!(
        world.receipt_statuses().iter().any(
            |status| matches!(status, WriteStatus::AwaitingRoute { detail } if detail.contains(&wanted))
        ),
        "the setup returned before its causal settled-absence receipt revision"
    );

    let queries_before_restart = world
        .indexer_names
        .iter()
        .map(|name| {
            (
                name.clone(),
                world.relays[name].query_count_for_kind(10_002),
            )
        })
        .collect::<Vec<_>>();

    world.stop_process().await;
    let path = world
        .store_path
        .clone()
        .expect("the durable setup must own a redb path");
    let store = RedbStore::open(path).expect("the stopped process releases its durable store");
    let recovered = store
        .recover_outbox()
        .expect("the accepted write must be recoverable");
    assert_eq!(recovered.len(), 1);
    assert!(
        store
            .recover_route_revisions(recovered[0].intent_id)
            .expect("route revisions must be readable")
            .is_empty(),
        "a zero-relay answer owns no durable route revision; boot must re-declare its route need"
    );
    drop(store);

    world.restart_engine(Some(ME.to_string())).await;
    let settled_after_restart = world.park_reason_contains(&wanted);
    let queries_after_restart = world
        .indexer_names
        .iter()
        .map(|name| {
            (
                name.clone(),
                world.relays[name].query_count_for_kind(10_002),
            )
        })
        .collect::<Vec<_>>();
    for ((before_name, before), (after_name, after)) in
        queries_before_restart.iter().zip(&queries_after_restart)
    {
        assert_eq!(before_name, after_name);
        assert!(
            after > before,
            "restart did not replay the recovered route need to indexer {after_name}: \
             before {before}, after {after}"
        );
    }
    assert!(
        settled_after_restart,
        "the observed settled-absence revision did not survive receipt reattachment; \
         indexer queries before restart {queries_before_restart:?}, after restart \
         {queries_after_restart:?}; receipt {:?}",
        world.park_reasons()
    );
}
