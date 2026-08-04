# Content and protocols

## Content packages

The base engine delivers raw event rows. Formatting and product policy remain app-owned.

Swift `NMPContent` and Kotlin's content SDK add source-ranged parsing, typed profile/article resources, and bounded reference sessions over ordinary NMP demand. Claims and nested sessions own live references; cancel Swift claims and `stop()` Swift sessions, and `close()` their Kotlin counterparts. They do not become another event store or routing authority.

Current Kotlin content snapshots do not fully report helper-query failures: the canonical collector maps a thrown query setup failure into a query-rejected shortfall, while helper collectors only catch cancellation and do not surface an ordinary or windowed canonical-projection failure (`ObservationUnavailable`) as a typed shortfall. Helper collectors run as async tasks on the shared engine runtime with no public capacity refusal, so keep aggregate observation bounded in app ownership for your own resource reasons and do not claim that every helper failure is visible in the snapshot.

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

A group can live on more than one relay at once, so the door is a scope named
once, narrowed to a group: `nmp::nip29::on(hosts)` returns a `RelayScope`
(fallible — `RelayScopeError::EmptyRelaySet` if the caller-supplied set is
empty), and `scope.group(groupId)` narrows it, keeping the same hosts. There
is no single-host constructor or free single-host discovery function — both
deleted, no alias.

NIP-29 does not supply a fixed group-content kind catalog: the app selects the
independently enabled schema kinds and calls `group.read(filter)`, which
scopes by `h` and returns one ordinary `LiveQuery` (one branch per scope host,
folded automatically — never a per-host list the app merges), taken through
the one `observe` door. Writes go through the same `Group`:
`group.publish(&engine, author, builder)` (plus `publish_signed` and the named
9000-9022 operations) preserves the draft's kind and schema, appends exactly
one `h` before signing, and routes `Explicit` to every host in the scope, not
one. Discovery across the scope is evidence-scoped:
`nip29::member_list_includes`/`admin_list_includes`/`any_of` build a
composable `GroupPredicate` (`union`/`intersect`/`minus`) over observed
kind:39002/39001 rows and over ids the app already knows; absence from a list
is never treated as proof of non-membership/non-admin.

Reading those records is `scope.observe(&engine, predicate, records)` and
`nip29::group(hosts, id)?.observe(&engine, records)`. Both deliver a complete
`GroupSnapshot` -- typed metadata plus the record's raw rows, admins and
members as `ListedSubject`s each carrying the hosts that named them, an
`availability` min'd over hosts, and the per-host breakdown beside the
aggregate. Roles come from 39001's third `p` position and are `Option<String>`,
never defaulted. Across hosts the lists union and the metadata does not: one
host's whole record wins on `created_at`, never merged field-wise.

Rust, FFI and both native SDKs project the full read-and-write door
(`FfiRelayScope`/`FfiGroup`/`FfiGroupPredicate`/`NmpGroupRecordsStream`;
`NMPRelayScope`/`NMPGroup`/`NMPGroupSnapshot` in Swift and Kotlin).

`nmp-nipc7` independently owns pure kind:9 chat and `q` replies. It does not
materialize mentions, notification `p` rows, NIP-29 `h`, or routing. No
Swift/Kotlin C7 projection is claimed yet.

When implementing a protocol feature not already projected, do not assemble it from mechanism crates in app code. First determine whether it belongs in an opt-in protocol crate and whether Rust/FFI/native surface governance is required.

Relay connection/worker failure during direct Rust NIP-02 observation is acquisition evidence, not `EngineError::ObservationUnavailable`; that error is reserved for an ordinary or windowed initial canonical-projection refusal after store degradation. The follow action has no capacity or thread refusal and reports any genuine terminal failure from `FollowAction` as `FollowActionStatus::Failed` with a `FollowActionFailure` variant. Raw UniFFI carries the same terminal action fact, Swift projects the matching `NMPFollowActionFailure`, and Kotlin still has no ergonomic following action.
