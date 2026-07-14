# Content and protocols

## Content packages

The base engine delivers raw event rows. Formatting and product policy remain app-owned.

Swift `NMPContent` and Kotlin's content SDK add source-ranged parsing, typed profile/article resources, and bounded reference sessions over ordinary NMP demand. Claims and nested sessions own live references; close/cancel them deterministically. They do not become another event store or routing authority.

Swift `NMPUI` adds replaceable SwiftUI components and renderer overrides above `NMPContent`. It includes identity primitives, mentions, event chrome, articles, user cards, reactions, a following button, and `NostrContent`. No Compose UI package is currently shipped.

## NIP-02 following

The current atomic follow/unfollow action is available in direct Rust protocol support and Swift. It first establishes an existing canonical contact-list base, preserves fields it does not own, and uses a replaceable precondition. It refuses a missing base; it does not silently create a first list containing only the new contact. The ergonomic Kotlin engine does not currently expose following actions.

## NIP-29 groups

NIP-29 helpers provide group discovery/content demand, remembered-group decoding, and a composed group-message intent pinned to the selected host. Swift/Kotlin apps obtain the pinned-host write transitively through protocol composition; they do not mint arbitrary pinned-host authority.

Kotlin's current call map is `groupContentDemand(host, groupId)` -> `NMPDemand`, `engine.observe(demand)` -> cold timeline flow, `NMPContentClient(engine).session(...)` -> `NostrContentSession`, and `session.claim(...)` -> closeable `NostrContentClaim`. Close claims before their session. For signer handoff use `engine.nip46Invitation(...)`, `engine.connectNip46(...)`, and `invitation.androidHandoff(signer)`; close the exact connection.

A composed intent is take-once. Compose a fresh intent for a new publication decision rather than reusing consumed state.

## NIP-46 and local signers

Swift and Kotlin expose NIP-46 invitations/connections and local-signer discovery metadata. The host owns OS handoff, package/scheme visibility, and UI. Start listening before handoff, wait for the connection's ready state, and close the connection explicitly. Swift's `connectNip46` overloads are `throws`; Kotlin maps the same synchronous raw bridge refusal through `NMPError.ThreadUnavailable`. If the outer bridge exists but the inner session/initial relay worker fails, Swift streams `.failed(reason:)` then finishes and Kotlin streams `Failed(reason)` then `Closed`; the public wrappers do not reconstruct `ThreadUnavailable` from that reason. Both paths are immediate failures, not signer-readiness timeouts.

Amber is NIP-55-only and is not a NIP-46 signer. Kotlin exposes Android handoff values, but the current JVM package does not execute NIP-55 or ship an Android integration layer.

When implementing a protocol feature not already projected, do not assemble it from mechanism crates in app code. First determine whether it belongs in an opt-in protocol crate and whether Rust/FFI/native surface governance is required.

Direct Rust NIP-02 observation setup returns `EngineError::ThreadUnavailable`, but action-worker refusal is not a synchronous `EngineError`: read it from `FollowAction` as `FollowActionStatus::Failed(FollowActionFailure::ThreadUnavailable { component, reason })`. Raw UniFFI carries the same terminal action fact, Swift projects it as `NMPFollowActionFailure.threadUnavailable(component:reason:)`, and Kotlin still has no ergonomic following action.
