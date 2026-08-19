# Platforms

## Direct Rust

Depend on the `nmp` crate and construct `Engine::new(EngineConfig)`, or `Engine::new_with_capabilities(EngineConfig, Vec<ReplaceableMaterializerSpec>)` to add a capability `nmp` cannot compile in itself. The consumer-facing methods are:

```text
reset_persistent_store
new
new_with_capabilities
new_with_session
observe
publish
cancel
publish_queue
publish_queue_for_event
remove_publish_queue_entry
reattach_receipt
receipt_result
reattach_by_correlation
session
export_session
add_private_key_account
add_public_key_account
make_current_account
remove_account
clear_session
sign_event
add_auth_policy
remove_auth_policy
observe_diagnostics
relay_information
shutdown
```

`new` supplies only the capabilities `nmp` compiles in itself (the NIP-29 group-list capability, when the `nip29` feature is enabled). A capability owned by a separate crate that depends on `nmp` — such as `nmp-nip02`'s follow/unfollow, which cannot be re-exported through the facade without a package cycle — is supplied explicitly: `new_with_capabilities(config, vec![nmp_nip02::follow_capability()])`. See [Content and protocols](content-and-protocols.md) for the full NIP-02 shape. A second spec for the same `(program, format)` pair refuses construction with `EngineError::DuplicateReplaceableCapability` rather than replacing the first.

`EngineConfig`'s fields are `store_path`, `app_relays`, `fallback_relays`, `max_relays`, `max_auth_capabilities`, `max_publish_attempts`, and `clock` (the engine's notion of now — `EngineClock::new()` is unpinned, so every read is the real system clock and an app with no opinion about time writes no clock code at all). There is no `indexer_relays` field: the exact operator sources for NIP-65 discovery are handed to the route provider instead, as `nmp_outbox::Nip65Outbox::new(sources)` passed to `Engine::new_with_capabilities_and_routing`. `max_publish_attempts` is how many failed attempts at ONE relay terminalise that lane as `RelayState::GaveUp` (default 16 — it counts observations, never wall-clock, so offline and AUTH-parked time spends nothing, and a write with no resolved route or no attached signer has no ceiling at all). There is no worker/task capacity field: #704 removed application-configurable task admission and all saturation outcomes. Observer/action/signer work runs as async tasks on one shared engine-owned runtime; private physical bounds backpressure rather than refusing ordinary operations. `EngineError::EngineStartFailed { component, reason }` is returned when the engine itself cannot be built (the OS refused an engine-owned thread, or the relay budget was unrepresentable) and is never raised by an ordinary operation once the engine exists — but it is not the only construction failure: `Engine::new` also reports `StoreOpenFailed`, `StoreAlreadyOpen`, `StoreUnsupportedSchema`, and `InvalidRelayUrl`. `AuthCapabilityRegistryFull { limit }` is a real capacity refusal, bounded by the app-set `max_auth_capabilities`; what does not exist is a worker/task ceiling.

`Engine::sign_event(SignEventRequest)` freezes the current session author and returns a cancellable `SignEventOperation`; `recv` yields one fully verified event or a typed `SignEventError`. It never accepts or publishes a write. The production session surface currently installs local-key providers; provider families may implement asynchronous capability work internally without exposing NMP's channels to consumers.

`Engine::relay_information(relay, policy)` is an async one-shot returning `RelayInformationSnapshot` or `RelayInformationRequestError`. `UseCache` returns an unexpired last-good representation; `Refresh` requests a generation-guarded single flight. Inspect `RelayInformationRequestError::Acquisition` without collapsing `ServiceClosed`, `CredentialedRelayUrl`, `Http`, `ResponseTooLarge`, or `InvalidDocument`. A stale-on-error success has `freshness: Stale` and `last_error`; `advertises_nip` is document evidence, not behavioral proof.

These infrastructure failures have distinct direct-Rust doors:

- `Engine::new` reports `EngineError::EngineStartFailed` when the engine itself cannot be constructed; no ordinary operation raises it. Store and relay-URL problems are their own variants.
- An ordinary or windowed `Engine::observe` reports `EngineError::ObservationUnavailable` only when store degradation prevents its initial canonical projection from opening. Relay connection/worker failure remains acquisition evidence. Window and `LiveQuery` validation refuse through their own variants. No OS thread is consumed per observation, and there is no worker/task-capacity refusal.
- `nmp_nip02::set_following` returns `Result<ReceiptStream, FollowActionFailure>`. Success is the ordinary durable receipt stream; signed-out, closed-engine, and pre-custody receipt failures are returned directly. It has no separate acquisition worker, retry lifecycle, capacity refusal, or thread refusal.

These are typed operational failures, not interchangeable error cases, a hidden task queue, panics, or timeouts. Every observer/action/signer path runs as an async task on the shared engine runtime, so ordinary concurrent operations simply make progress.
