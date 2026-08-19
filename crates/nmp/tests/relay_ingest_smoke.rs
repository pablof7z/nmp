#![recursion_limit = "512"]

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
        queue_capacity: 8,
        verified_cache_capacity: 257,
        committed_observation_cache_capacity: 1_028,
        diagnostic_duplicate_ceiling_capacity: 0,
        diagnostic_duplicate_ceiling_event_payload: false,
        diagnostic_preparsed_ceiling: false,
        diagnostic_skip_event_id_validation: false,
        diagnostic_skip_signature_verification: false,
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
        completion_window_output: None,
    })
    .expect("end-to-end relay ingest smoke");

    assert_eq!(result.expected_relay_frames, 1_028);
    assert_eq!(result.observed_relay_frames, 1_028);
    assert_eq!(result.final_visible_rows, 64);
    assert_eq!(result.delivery_mode, "bounded-latest-window");
    assert!(result.database_bytes > 0);
    assert_eq!(result.server_send_ms.len(), 2);
    assert_eq!(result.server_bytes.len(), 2);
    #[cfg(feature = "bench-instrumentation")]
    {
        let attribution = result.ingest_attribution.expect("bench attribution");
        assert_eq!(attribution["transport"]["committed_observation_hits"], 514);
        assert_eq!(attribution["resolver"]["events"], 514);
        assert!(attribution["engine"]["bridge_batches"].as_u64().unwrap() < 400);
    }
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
        queue_capacity: 128,
        verified_cache_capacity: 65,
        committed_observation_cache_capacity: 0,
        diagnostic_duplicate_ceiling_capacity: 65,
        diagnostic_duplicate_ceiling_event_payload: true,
        diagnostic_preparsed_ceiling: false,
        diagnostic_skip_event_id_validation: false,
        diagnostic_skip_signature_verification: false,
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
        completion_window_output: None,
    })
    .expect("diagnostic duplicate ceiling smoke");

    assert_eq!(result.observed_relay_frames, 130);
    let attribution = result.ingest_attribution.expect("bench attribution");
    assert_eq!(
        attribution["transport"]["diagnostic_duplicate_ceiling_hits"],
        65
    );
    assert_eq!(attribution["resolver"]["events"], 65);
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
        queue_capacity: 8,
        verified_cache_capacity: 1,
        committed_observation_cache_capacity: 1,
        diagnostic_duplicate_ceiling_capacity: 0,
        diagnostic_duplicate_ceiling_event_payload: false,
        diagnostic_preparsed_ceiling: false,
        diagnostic_skip_event_id_validation: false,
        diagnostic_skip_signature_verification: false,
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
        completion_window_output: None,
    })
    .expect("oversize relay message is rejected end to end");

    assert_eq!(result.expected_relay_frames, 1);
    assert_eq!(result.observed_relay_frames, 0);
    assert_eq!(result.observed_added_rows, 0);
    assert_eq!(result.final_visible_rows, 0);
}
