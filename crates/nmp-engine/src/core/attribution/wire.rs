//! Wire subscription-id rendering for attribution and transport.

use nmp_router::SubId;

/// The wire-format `subscription_id` string `EngineCore` sends a REQ under
/// for `sub_id`: the hex `Display` of its `SubId.1` digest — 64 lowercase hex
/// characters, exactly NIP-01's `subscription_id` cap, never prefixed and
/// never truncated. This is an internal implementation detail EngineCore owns
/// end-to-end (recorded at send time in `sub_id_by_wire`, read back at EOSE
/// time from the same map) — nothing else in the M3 crate graph has committed
/// to a different convention, so no other component's contract depends on
/// this exact format.
///
/// Since #899 a PLANNED sub's digest is an allocated opaque token, not a hash
/// of its filter (`nmp-router`'s `SubId::allocate`), so nothing here — or
/// anywhere else — may re-derive a wire id from a filter. The map is the only
/// authority in both directions, which it already was.
pub fn wire_sub_id_string(sub_id: &SubId) -> String {
    sub_id.1.to_string()
}
