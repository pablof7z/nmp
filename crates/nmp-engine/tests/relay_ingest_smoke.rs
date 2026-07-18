#[path = "../examples/support/relay_ingest_probe.rs"]
mod relay_ingest_probe;

use std::time::Duration;

use relay_ingest_probe::ProbeConfig;

#[test]
fn websocket_runtime_to_redb_smoke_crosses_every_bounded_queue() {
    let result = relay_ingest_probe::run(ProbeConfig {
        events: 257,
        relays: 2,
        passes: 2,
        payload_bytes: 256,
        shape_corpus: None,
        corpus_output: None,
        memory_store: false,
        redb_nondurable_diagnostic: false,
        queue_capacity: 8,
        verified_cache_capacity: 257,
        diagnostic_duplicate_ceiling_capacity: 0,
        diagnostic_duplicate_ceiling_event_payload: false,
        verifier_workers: 0,
        verify_batch_size: 7,
        engine_batch_size: 7,
        engine_batch_bytes: 8 * 1024 * 1024,
        engine_batch_wait: Duration::ZERO,
        visible_limit: Some(64),
        trim_allocator_during_ingest: false,
        frame_delay: Duration::ZERO,
        expect_rejection: false,
        timeout: Duration::from_secs(30),
        store_path: None,
    })
    .expect("end-to-end relay ingest smoke");

    assert_eq!(result.expected_relay_frames, 1_028);
    assert_eq!(result.observed_relay_frames, 1_028);
    assert_eq!(result.final_visible_rows, 64);
    assert_eq!(result.delivery_mode, "bounded-latest-window");
    assert!(result.database_bytes > 0);
    assert_eq!(result.server_send_ms.len(), 2);
    assert_eq!(result.server_bytes.len(), 2);
}

#[test]
fn websocket_runtime_to_memory_store_pins_the_no_persistence_ceiling() {
    let result = relay_ingest_probe::run(ProbeConfig {
        events: 65,
        relays: 1,
        passes: 1,
        payload_bytes: 128,
        shape_corpus: None,
        corpus_output: None,
        memory_store: true,
        redb_nondurable_diagnostic: false,
        queue_capacity: 8,
        verified_cache_capacity: 65,
        diagnostic_duplicate_ceiling_capacity: 0,
        diagnostic_duplicate_ceiling_event_payload: false,
        verifier_workers: 0,
        verify_batch_size: 7,
        engine_batch_size: 7,
        engine_batch_bytes: 8 * 1024 * 1024,
        engine_batch_wait: Duration::ZERO,
        visible_limit: Some(32),
        trim_allocator_during_ingest: false,
        frame_delay: Duration::ZERO,
        expect_rejection: false,
        timeout: Duration::from_secs(30),
        store_path: None,
    })
    .expect("end-to-end memory-store ceiling smoke");

    assert_eq!(result.store_backend, "memory");
    assert_eq!(result.expected_relay_frames, 65);
    assert_eq!(result.observed_relay_frames, 65);
    assert_eq!(result.final_visible_rows, 32);
    assert_eq!(result.database_bytes, 0);
    assert_eq!(result.reopen_and_verify_ms, 0.0);
}

#[cfg(feature = "bench-instrumentation")]
#[test]
fn nondurable_redb_diagnostic_finishes_with_a_timed_durable_checkpoint() {
    let result = relay_ingest_probe::run(ProbeConfig {
        events: 65,
        relays: 1,
        passes: 1,
        payload_bytes: 128,
        shape_corpus: None,
        corpus_output: None,
        memory_store: false,
        redb_nondurable_diagnostic: true,
        queue_capacity: 8,
        verified_cache_capacity: 65,
        diagnostic_duplicate_ceiling_capacity: 0,
        diagnostic_duplicate_ceiling_event_payload: false,
        verifier_workers: 0,
        verify_batch_size: 7,
        engine_batch_size: 7,
        engine_batch_bytes: 8 * 1024 * 1024,
        engine_batch_wait: Duration::ZERO,
        visible_limit: Some(32),
        trim_allocator_during_ingest: false,
        frame_delay: Duration::ZERO,
        expect_rejection: false,
        timeout: Duration::from_secs(30),
        store_path: None,
    })
    .expect("nondurable Redb diagnostic smoke");

    assert_eq!(
        result.store_durability,
        "none-then-immediate-checkpoint-diagnostic"
    );
    assert_eq!(result.observed_relay_frames, 65);
    assert_eq!(result.final_visible_rows, 32);
    assert!(result.reopen_and_verify_ms > 0.0);
    assert!(
        result.ingest_attribution.as_ref().unwrap()["store"]["durability_checkpoint_ns"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[cfg(feature = "bench-instrumentation")]
#[test]
fn duplicate_ceiling_bypasses_second_pass_parse_resolver_and_store_work() {
    let result = relay_ingest_probe::run(ProbeConfig {
        events: 65,
        relays: 1,
        passes: 2,
        payload_bytes: 128,
        shape_corpus: None,
        corpus_output: None,
        memory_store: false,
        redb_nondurable_diagnostic: false,
        queue_capacity: 128,
        verified_cache_capacity: 65,
        diagnostic_duplicate_ceiling_capacity: 65,
        diagnostic_duplicate_ceiling_event_payload: true,
        verifier_workers: 0,
        verify_batch_size: 64,
        engine_batch_size: 64,
        engine_batch_bytes: 8 * 1024 * 1024,
        engine_batch_wait: Duration::from_micros(50),
        visible_limit: Some(32),
        trim_allocator_during_ingest: false,
        frame_delay: Duration::ZERO,
        expect_rejection: false,
        timeout: Duration::from_secs(30),
        store_path: None,
    })
    .expect("diagnostic duplicate ceiling smoke");

    assert_eq!(result.observed_relay_frames, 130);
    let attribution = result.ingest_attribution.expect("bench attribution");
    assert_eq!(
        attribution["transport"]["diagnostic_duplicate_ceiling_hits"],
        65
    );
    assert_eq!(attribution["resolver"]["events"], 65);
    assert_eq!(attribution["store"]["events"], 65);
}

#[test]
fn websocket_runtime_rejects_a_message_above_the_one_mib_ceiling() {
    let result = relay_ingest_probe::run(ProbeConfig {
        events: 1,
        relays: 1,
        passes: 1,
        payload_bytes: 1_049_000,
        shape_corpus: None,
        corpus_output: None,
        memory_store: false,
        redb_nondurable_diagnostic: false,
        queue_capacity: 8,
        verified_cache_capacity: 1,
        diagnostic_duplicate_ceiling_capacity: 0,
        diagnostic_duplicate_ceiling_event_payload: false,
        verifier_workers: 0,
        verify_batch_size: 7,
        engine_batch_size: 7,
        engine_batch_bytes: 8 * 1024 * 1024,
        engine_batch_wait: Duration::ZERO,
        visible_limit: Some(64),
        trim_allocator_during_ingest: false,
        frame_delay: Duration::ZERO,
        expect_rejection: true,
        timeout: Duration::from_secs(30),
        store_path: None,
    })
    .expect("oversize relay message is rejected end to end");

    assert_eq!(result.expected_relay_frames, 1);
    assert_eq!(result.observed_relay_frames, 0);
    assert_eq!(result.observed_added_rows, 0);
    assert_eq!(result.final_visible_rows, 0);
}
