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
        queue_capacity: 8,
        verified_cache_capacity: 257,
        verify_batch_size: 7,
        engine_batch_size: 7,
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
fn websocket_runtime_rejects_a_message_above_the_one_mib_ceiling() {
    let result = relay_ingest_probe::run(ProbeConfig {
        events: 1,
        relays: 1,
        passes: 1,
        payload_bytes: 1_049_000,
        queue_capacity: 8,
        verified_cache_capacity: 1,
        verify_batch_size: 7,
        engine_batch_size: 7,
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
