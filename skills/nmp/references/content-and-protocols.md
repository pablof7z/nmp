# Content and protocols

## Content packages

The base engine delivers raw event rows. Formatting and product policy remain app-owned.

Swift `NMPContent` and Kotlin's content SDK add source-ranged parsing, typed profile/article resources, and bounded reference sessions over ordinary NMP demand. Claims and nested sessions own live references; cancel Swift claims and `stop()` Swift sessions, and `close()` their Kotlin counterparts. They do not become another event store or routing authority.

Current Kotlin content snapshots do not fully report helper-query failures: the canonical collector maps a thrown query setup failure into a query-rejected shortfall, while helper collectors only catch cancellation and do not surface a windowed canonical-projection failure (`ObservationUnavailable`) as a typed shortfall. Helper collectors run as async tasks on the shared engine runtime with no public capacity refusal, so keep aggregate observation bounded in app ownership for your own resource reasons and do not claim that every helper failure is visible in the snapshot.

Swift `NMPUI` adds replaceable SwiftUI components and renderer overrides above `NMPContent`. It includes identity primitives, mentions, event chrome, articles, user cards, reactions, a following button, and `NostrContent`. No Compose UI package is currently shipped.

## NIP-02 following

The current atomic follow/unfollow action is available in direct Rust protocol support and Swift. It first establishes an existing canonical contact-list base, preserves fields it does not own, and uses a replaceable precondition. It refuses a missing base; it does not silently create a first list containing only the new contact. The ergonomic Kotlin engine does not currently expose following actions.

## NIP-22 comments

NIP-22 owns typed kind:1111 comments over event, address, and NIP-73 external
roots. Rust, FFI, Swift, and Kotlin expose root-thread demand, strict decode,
and schema composition. Composition names no author and reads no clock: the
engine resolves the write's identity and stamps `created_at` at acceptance,
so two composes of the same comment are two valid events, not one repeated
one.

The native composer is the top-level
`commentIntent(root:parent:content:correlation:)`.
It returns the ordinary `WriteIntent` with durable author-outbox routing.
Publish that value through the generic engine `publish` door and observe its
ordinary receipt. There is no `engine.commentIntent`, `CommentIntent` wrapper,
NIP-22 `publishComposed`, or take-once lifecycle.

## NIP-29 groups

Swift/Kotlin currently project only `groupDiscoveryDemand(host)`. NIP-29 does
not supply a fixed group-content kind catalog: the app selects the independently
enabled schema kinds and builds an ordinary `NMPDemand` scoped by `h` and a
pinned source. Direct Rust has the full door: `nip29::Group::new(host, groupId)`
is an identity that mints both a read `Demand` (`group.demand(filter)`, taken
through the one `observe` door) and every write
(`group.publish(&engine, builder)`, plus the named 9000-9022 operations). It
preserves the draft's kind and schema, appends exactly one `h` before signing,
and routes explicitly to the host. There is no native/Swift/Kotlin projection
of group publication yet.

`nmp-nipc7` independently owns pure kind:9 chat and `q` replies. It does not
materialize mentions, notification `p` rows, NIP-29 `h`, or routing. No
Swift/Kotlin C7 projection is claimed yet.

## NIP-46 and local signers

Swift and Kotlin expose NIP-46 invitations/connections and local-signer discovery metadata. The host owns OS handoff, package/scheme visibility, and UI. Materialize the handoff URI/value before invitation connection consumes the invitation; then connect, start listening, and only then launch the cached handoff. Wait for the connection's ready state and close the connection explicitly. Swift's `connectNip46` overloads are `throws`; NIP-46 connection has no capacity or thread refusal, and a genuine relay/session setup failure surfaces as a typed `NMPNip46Failure`/`Nip46Error`. If a handle returns but the inner session/initial relay worker fails, Swift streams `.failed(reason:)` then finishes and Kotlin streams `Failed(reason)` then `Closed`; the public wrappers do not reconstruct a typed refusal from that reason. Both paths are immediate failures, not signer-readiness timeouts.

Amber is NIP-55-only and is not a NIP-46 signer. Kotlin exposes Android handoff values, but the current JVM package does not execute NIP-55 or ship an Android integration layer.

When implementing a protocol feature not already projected, do not assemble it from mechanism crates in app code. First determine whether it belongs in an opt-in protocol crate and whether Rust/FFI/native surface governance is required.

Relay connection/worker failure during direct Rust NIP-02 observation is acquisition evidence, not `EngineError::ObservationUnavailable`; that error is reserved for canonical windowed history-projection setup after store degradation. The follow action has no capacity or thread refusal and reports any genuine terminal failure from `FollowAction` as `FollowActionStatus::Failed` with a `FollowActionFailure` variant. Raw UniFFI carries the same terminal action fact, Swift projects the matching `NMPFollowActionFailure`, and Kotlin still has no ergonomic following action.
