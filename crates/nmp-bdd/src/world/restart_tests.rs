//! Deterministic falsifier for the settled-route process-boundary ordering.
//!
//! A fast local indexer used to let the past-tense publish setup return
//! before its causal EOSE by accident. Delaying query admission makes that
//! ordering explicit: the setup must not return until the receipt contains
//! the settled `Absent` revision that the restart is meant to preserve.
//!
//! What survives that boundary changed with the owner's 2026-08-04 ruling.
//! A settled absence is knowledge EXHAUSTED, so the write terminates as
//! `NoDestination` rather than parking on a question already answered. The
//! open-work row is therefore reclaimed and boot re-declares NOTHING for it —
//! the fact that must survive is the terminal itself, retained on the receipt
//! and replayed on reattachment.

use std::time::Duration;

use nmp::mechanism::publish_queue::{WriteFact, WriteOutcome};
use nmp_store::RedbStore;

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

    assert!(
        world
            .receipt_statuses()
            .iter()
            .any(|status| matches!(status, WriteFact::Outcome(WriteOutcome::NoDestination))),
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
        .recover_publish_queue()
        .expect("the publish queue must be readable");
    assert!(
        recovered.is_empty(),
        "a settled absence is knowledge exhausted, so the write is terminal and owns no open \
         work; leaving its row behind would replay an answered question on every boot and \
         strand an entry the removal door refuses: {recovered:?}"
    );
    drop(store);

    world.restart_engine(Some(ME.to_string())).await;
    let settled_after_restart = world.no_destination_settled();
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
        assert_eq!(
            after, before,
            "restart re-declared a route need for a write whose routing already finished; \
             a terminated write must not keep discovery alive on indexer {after_name}"
        );
    }
    assert!(
        settled_after_restart,
        "the observed settled-absence terminal did not survive receipt reattachment; \
         indexer queries before restart {queries_before_restart:?}, after restart \
         {queries_after_restart:?}; receipt {:?}",
        world.routing_facts_reported()
    );
}
