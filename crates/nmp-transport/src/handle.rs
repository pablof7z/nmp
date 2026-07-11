//! Generational relay handles (M3 plan §3.2, ledger #2/#3/#4).

/// A generational handle to a pooled relay connection. A handle tagged
/// with a superseded generation is structurally rejected by [`crate::Pool`]:
/// `send`/`close` on a stale handle return `false`, and an inbound frame
/// tagged with an old generation is dropped by the pool translator before
/// it ever reaches the engine's `EngineMsg::RelayFrame`. Reconnect always
/// mints a NEW generation for the same slot.
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub struct RelayHandle {
    pub slot: u32,
    pub generation: u64,
}
