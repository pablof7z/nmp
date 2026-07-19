# Content and protocols

## Content packages

The base engine delivers raw event rows. Formatting and product policy remain app-owned.

Swift `NMPContent` and Kotlin's content SDK add source-ranged parsing, typed profile/article resources, and bounded reference sessions over ordinary NMP demand. Claims and nested sessions own live references; cancel Swift claims and `stop()` Swift sessions, and `close()` their Kotlin counterparts. They do not become another event store or routing authority.

Current Kotlin content snapshots do not fully report helper-query failures: the canonical collector maps a thrown query setup failure into a query-rejected shortfall, while helper collectors only catch cancellation and do not surface a live-observe infra failure (`ObservationUnavailable`) as a typed shortfall. Helper collectors run as async tasks on the shared engine runtime with no admission ceiling, so keep aggregate observation bounded in app ownership for your own resource reasons and do not claim that every helper failure is visible in the snapshot.

Swift `NMPUI` adds replaceable SwiftUI components and renderer overrides above `NMPContent`. It includes identity primitives, mentions, event chrome, articles, user cards, reactions, a following button, and `NostrContent`. No Compose UI package is currently shipped.

## NIP-02 following

The current atomic follow/unfollow action is available in direct Rust protocol support and Swift. It first establishes an existing canonical contact-list base, preserves fields it does not own, and uses a replaceable precondition. It refuses a missing base; it does not silently create a first list containing only the new contact. The ergonomic Kotlin engine does not currently expose following actions.

## NIP-29 groups

NIP-29 helpers provide group discovery/content demand, remembered-group decoding, and a composed group-message intent pinned to the selected host. Swift/Kotlin apps obtain the pinned-host write transitively through protocol composition; they do not mint arbitrary pinned-host authority.

Kotlin's current call map is `groupContentDemand(host, groupId)` -> `NMPDemand`, `engine.observe(demand)` -> cold timeline flow, `NMPContentClient(engine).session(...)` -> `NostrContentSession`, and `session.claim(...)` -> closeable `NostrContentClaim` for a parsed reference. Close claims before their session. For signer handoff use `engine.nip46Invitation(...)`, derive and cache `invitation.androidHandoff(signer)` while the invitation is still live, then call `engine.connectNip46(...)`, start state collection, and launch the cached explicit handoff. Wait for `Ready`, activate that user pubkey before unsigned writes, and close the exact connection.

A composed intent is take-once. Compose a fresh intent for a new publication decision rather than reusing consumed state.

## NIP-46 and local signers

Swift and Kotlin expose NIP-46 invitations/connections and local-signer discovery metadata. The host owns OS handoff, package/scheme visibility, and UI. Materialize the handoff URI/value before invitation connection consumes the invitation; then connect, start listening, and only then launch the cached handoff. Wait for the connection's ready state and close the connection explicitly. Swift's `connectNip46` overloads are `throws`; NIP-46 connection has no capacity or thread refusal, and a genuine relay/session setup failure surfaces as a typed `NMPNip46Failure`/`Nip46Error`. If a handle returns but the inner session/initial relay worker fails, Swift streams `.failed(reason:)` then finishes and Kotlin streams `Failed(reason)` then `Closed`; the public wrappers do not reconstruct a typed refusal from that reason. Both paths are immediate failures, not signer-readiness timeouts.

Amber is NIP-55-only and is not a NIP-46 signer. Kotlin exposes Android handoff values, but the current JVM package does not execute NIP-55 or ship an Android integration layer.

When implementing a protocol feature not already projected, do not assemble it from mechanism crates in app code. First determine whether it belongs in an opt-in protocol crate and whether Rust/FFI/native surface governance is required.

Direct Rust NIP-02 observation setup can return `EngineError::ObservationUnavailable` only when a live observation cannot open its relay connection; the follow action has no capacity or thread refusal and reports any genuine terminal failure from `FollowAction` as `FollowActionStatus::Failed` with a `FollowActionFailure` variant. Raw UniFFI carries the same terminal action fact, Swift projects the matching `NMPFollowActionFailure`, and Kotlin still has no ergonomic following action.
