# Content and protocols

## Content packages

The base engine delivers raw event rows. Formatting and product policy remain app-owned.

Content support is parser-only on every tier: `nmp::content::parse_content` in Rust, `parseNostrContent` in Swift's `NMPContent` and in the Kotlin SDK. Each returns an immutable source-ranged document. Parsing is pure — it owns no protocol schema, renderer, component registry, query handle, cache, engine, or network client, and it holds nothing to close.

There is no typed profile/article resource layer and no bounded reference-session machinery: no `NMPContentClient`, no `NostrContentSession`, no `claim(referenceID:)`, and no session permit budget. To make a nested reference live, resolve its locator from the parsed document and open an ordinary query the app owns.

Swift `NMPUI` adds replaceable SwiftUI components and renderer overrides above `NMPContent`. It includes identity primitives, mentions, event chrome, articles, user cards, reactions, relay views, `NMPFollowButton`, and `NostrContent`. It decodes nothing itself: `NMPArticlePortraitCard`/`NMPArticleMediumCard` render an app-constructed `NMPArticlePresentation`, so no NIP-23 decode ships. Nested references are opened through `NMPReferenceObservationFactory` — an app-supplied seam that receives the authored locator and returns an `NMPReferenceObservationHandle`; NMPUI never chooses kind, source authority, cache or freshness on the app's behalf. On the Kotlin side a narrow optional desktop-JVM Compose library ships as the separate `:ui` child project (`com.nmp.ui`), carrying relay identity/list primitives only; broad Compose parity and a Compose gallery are unbuilt, and it is not an Android AAR.

## NIP-02 following

The atomic follow/unfollow action lives in the `nmp-nip02` protocol crate and is projected to Swift and Kotlin. `nmp-nip02` depends on `nmp`, so it is not and cannot be re-exported through the facade — an `nmp -> nmp-nip02` edge is a package cycle cargo refuses; `nmp-ffi` binds it directly. `set_following(engine, target, change)` takes a decoded `PublicKey`. It first establishes an existing canonical contact-list base, preserves fields it does not own, and pins `expected_base` on a `WritePayload::ReplaceableEdit`. It refuses a missing base (`FollowActionFailure::NoContactList`); it does not silently create a first list containing only the new contact. A stale base surfaces as an ordinary receipt refusal (`ReplaceableBaseChanged`), which a second attempt can fix.

Beside the action, `nmp-nip02` exposes a registration-bound composer for apps that want the operation shape rather than the whole action: `register_follow_writes(&engine)` returns `FollowWrites`, whose `compose(original_source, current, target, change)` returns `ComposeFollowResult::NoChange` or `ComposeFollowResult::Publish(Box<WriteIntent>)`. The published intent carries the registered replaceable operation replayed by NMP's own materializer. Without the registration there is no operation door at all.

Swift surfaces `observeFollowing(_:)`/`follow(_:)`/`unfollow(_:)` as `NMPEngine` extension methods returning `NMPFollowingObservation`/`NMPFollowAction`, with the typed `NMPFollowActionStatus`/`NMPFollowActionFailure` mirrors. Kotlin surfaces the same three as top-level functions taking the engine — `observeFollowing(engine, target)`, `follow(engine, target)`, `unfollow(engine, target)` — returning a `Flow<FollowingSnapshot>` and a `FollowAction`, with unprefixed `FollowActionStatus`/`FollowActionFailure`. The one Swift-only convenience is the Combine `NMPFollowing` observable object; Kotlin has no counterpart.

## NIP-22 comments

NIP-22 owns typed kind:1111 comments. Composition takes a single target and
never a separate root and parent: the root is read off the target's own rows,
so commenting on a thread root and replying to a deep comment are the same
call and cannot be got backwards. Rust, FFI, Swift, and Kotlin expose
root-thread demand (`comment_thread_demand`/`commentThreadDemand`), strict
decode (`decode_comment`/`decodeComment`, with an exhaustive
`CommentDecodeError`), and schema composition. Composition names no author and
reads no clock — the intent carries `Identity::Active` and no `created_at`, so
the engine resolves identity and stamps time at acceptance, and two composes
of the same comment are two valid events rather than one repeated one.

In direct Rust the target is anything implementing `nmp_grammar::RootScope`:
`comment_intent(&target, content, correlation)` accepts a `CommentRoot` (event,
address, or NIP-73 external), a delivered `Row`, a signed `Event`, or a `Nip73`
value directly. At the FFI and native boundary the same choice is a closed
`CommentTarget` enum — `.root(CommentRoot)` or `.row(Row)` — because a
generic bound does not cross UniFFI.

The native composer is the top-level Swift
`commentIntent(on:content:correlation:)` / Kotlin
`commentIntent(target, content, correlation)`. It returns the ordinary
`WriteIntent` on the `Auto` route, so durable author-outbox routing applies.
Publish that value through the generic engine `publish` door and observe its
ordinary receipt. There is no `engine.commentIntent`, `CommentIntent` wrapper,
NIP-22 `publishComposed`, or take-once lifecycle.

## NIP-29 groups

A group can live on more than one relay at once, so the door is a scope named
once, narrowed to a group: `nmp::nip29::on(hosts)` returns a `RelayScope`
(fallible — `RelayScopeError::EmptyRelaySet` if the caller-supplied set is
empty), and `scope.group(groupId)` narrows it, keeping the same hosts. `on`
takes decoded `RelayUrl`s, never pasted strings. There is no single-host
constructor and no free single-host discovery function. `nip29::group(hosts,
id)` is sugar for `on(hosts)?.group(id)` and nothing more.

`scope.groups(ids)` is the write-only sibling: it narrows to the SEVERAL groups
one event belongs to, for the one shape a single group id cannot express — an
addressable event such as a kind:30315 session status that carries one `h` per
room, where publishing once per room would make each copy replace the last. It
is fallible for the same reason `on` is (`GroupContextError::NoGroupNamed`),
and it is a write context only: no read, no records, no named operation, since
each of those is per-group by definition.

NIP-29 does not supply a fixed group-content kind catalog: the app selects the
independently enabled schema kinds and calls `group.read(filter)`, which
scopes by `h` and returns one ordinary `LiveQuery` (one branch per scope host,
folded automatically — never a per-host list the app merges), taken through
the one `observe` door. Writes go through the same `Group`:
`group.publish(&engine, author, builder)` preserves the draft's kind and
schema, appends exactly one `h` before signing, and routes `Explicit` to every
host in the scope, not one. `author` is an exact decoded `PublicKey`, frozen at
composition time, never a reactive selector. Named operations sit beside it and
compose the kinds NIP-29 defines: `join_request` (9021), `leave_request`
(9022), `add_users` (9000), `remove_users` (9001), `edit_metadata` (9002),
`delete_event` (9005), `create_group` (9007, optionally naming a parent group
id for a subgroup), `delete_group` (9008), `create_invite` (9009). All return
the ordinary `ReceiptStream`. `Group::publish` and `Groups::publish` are the
only write doors: the intent mint is private, so there is no
mint-without-publish door and no pre-signed group publication. An app that
needs a signed event WITHOUT publishing it calls `Engine::sign_event`, which
creates no write intent, receipt or publication and hands back the signed
event; `group.validate_context(&event)` separately answers whether an
already-signed event belongs to this group without building a write from it.

Discovery across the scope is the ordinary query language, not a closed leaf
set: `nip29::groups_whose_record_matches(Filter)` names the groups whose own
relay-signed record matches an ordinary live-query filter at the branch host.
It is fallible — the selection is evaluated with NIP-29's own authority (pinned
to the branch host, `CacheMode::Strict`), so it must name at least one kind
(`GroupPredicateError::NoKindSelected`) and only NIP-29's three relay-signed
record kinds (`NotAGroupRecordKind`). `member_list_includes` /
`admin_list_includes` are infallible shorthands over it, exactly equal to
writing it out against kind:39002 / kind:39001 with `#p`. `any_of(Binding)`
takes any binding, so "the groups named in my own kind:10009 list" is a derived
source that stays reactive and keeps its OWN authority — lowering never repins
it. All four build a `GroupIds`, composable with `union`/`intersect`/`minus`.
Absence from a list is never treated as proof of non-membership/non-admin.

`nip29::all()` is the fifth spelling and the only one that is not a
`GroupIds`: every group the host advertises, expressed as the ABSENCE of a
`#d` row. It is unbounded by nature -- bound it with `observe`'s own per-host
`limit`, which is the ordinary NIP-01 `Filter::limit` applied per branch and
never promoted to a bound on the merged union -- and advertisement is not
enumeration: a group the host serves but publishes no kind:39000 for is
invisible. Set algebra is on `GroupIds` alone, so `all().minus(...)` does not
typecheck: Nostr filters have no negation, and "everything except X" cannot
narrow a wire request. Filter muted rooms out of the `Vec<GroupSnapshot>` you
render.

Reading those records is `scope.observe(&engine, predicate, records, limit)` and
`nip29::group(hosts, id)?.observe(&engine, records)`. Both refuse an empty
record selection (`GroupObserveError::NoRecordSelected`) rather than opening an
observation that can only ever be empty. Both deliver a complete
`GroupSnapshot` -- typed metadata, admins and members as `ListedSubject`s each
carrying the hosts that named them, an `availability` min'd over hosts, and the
per-host `HostRecords` breakdown beside the aggregate, reachable by
`snapshot.at(host)`. A `ListedSubject` carries a decoded `PublicKey`, never a
bech32 string. Its `role` is the cell the relay wrote beside the subject in its
own list record, `Option<String>`, never defaulted. Across hosts the lists
union and the metadata does not: one host's whole record wins on `created_at`
(ties broken by event id), never merged field-wise. The snapshot also carries
`disagreements`, and `snapshot.differs(record)` reports whether the answering
hosts disagree about one -- enough for an app to offer a dig-in affordance
without walking the per-host map itself. Dropping the `GroupObservation`
withdraws the demand; NMP retains nothing keyed by group.

Rust, FFI and both native SDKs project the full read-and-write door
(`FfiRelayScope`/`FfiGroup`/`FfiGroups`/`FfiGroupPredicate`/`FfiGroupIds`/`NmpGroupRecordsStream`;
`NMPRelayScope`/`NMPGroup`/`NMPGroups`/`NMPGroupPredicate`/`NMPGroupIds`/`NMPGroupSnapshot`
in Swift and Kotlin). The native record read is spelled `observeRecords` on
both the scope and the group.

`nmp-nipc7` independently owns pure kind:9 chat, and its replies emit `e`, not
`q` — a kind:9 must not become a NIP-22 comment, because NIP-29 clients fetch
only kind 9. It composes schema only: no mentions, no notification `p` rows, no
NIP-29 `h`, no routing, and no content. When the message belongs to a group,
hand the composed draft to `Group::publish` (`FfiGroup::publish` /
`NMPGroup.publish` natively), which is what supplies the `h` row and the hosts.

## The one tagging door

Composition for the reply-shaped families is one FFI/native module rather than
one door per protocol crate. Rust exposes each composer under its own facade
module (`nmp::nipc7::chat`/`chat_reply`, `nmp::nip18::repost`,
`nmp::nip25::react`, `nmp::reply_to`); every one of them returns an
`EventBuilder`, names no author and reads no clock. FFI and both wrappers
project them as free functions returning a `WritePayload`. Each target-taking
one takes the `Row` the app already holds and nothing else — never a
relationship, marker, relay hint or author, which the door fills from the row's
own `sources`:

- `chat()` — top-level NIP-C7 kind:9.
- `chatReply(to:)` in Swift, `chatReply(target)` in Kotlin — kind:9 with the `e` row.
- `replyTo(_:)` — the ordinary reply: NIP-10 for a text note, a NIP-22 comment for everything else.
- `repost(_:)` — NIP-18 picks kind:6 or kind:16 itself; a caller never states a kind.
- `react(to:with:)` in Swift, `react(target, reaction)` in Kotlin — NIP-25, taking a closed `Reaction` (`.like`/`.dislike`/`.emoji`) rather than a raw content string, since NIP-25 assigns `+`, `-` and the empty string fixed meanings.
- `.withContent([ContentPart])` — states what a draft says AND emits the rows its inline references need from the same call, so a `nostr:npub…` token and its `p` row cannot be written apart. Parts are `.text`, `.person(pubkey:relay:)` and `.quote(Row)`. Rows are appended after whatever the composer already stated, never reordered or deduplicated against them.

The facade also carries NIP-51 simple-groups lists (`current_account_demand`
plus the tolerant parsers, projected as `currentAccountDemand()` and
`parseSimpleGroupsListTolerant(_:)`), Blossom blob storage, and exact-byte
asset identity, each behind its own cargo feature (`nip18`, `nip22`, `nip25`,
`nip29`, `nip51`, `nip65`, `nipc7`, `content`, `asset`, `blossom`; `blossom`
turns on `asset`). All are non-default. Blossom's client verbs are upload,
mirror, delete and list, each with its own typed error and its own unsigned
authorization draft the caller signs through the ordinary signing path; the
client is engine-free and is not a second way to publish. Everything above
except NIP-65 reaches both wrappers. NIP-65 relay-list bootstrap
(`publish_relay_list_bootstrap`, `relay_list_demand`) is direct-Rust only.
There is no NIP-23 owner at all — no crate, no `Article` type, no decode — so
an article feature must add that owner from scratch.

When implementing a protocol feature not already projected, do not assemble it from mechanism crates in app code. First determine whether it belongs in an opt-in protocol crate and whether Rust/FFI/native API projection is required.

Relay connection/worker failure during direct Rust NIP-02 observation is acquisition evidence, not `EngineError::ObservationUnavailable`; that error is reserved for an ordinary or windowed initial canonical-projection refusal after store degradation. The follow action has no worker/task refusal and reports any genuine terminal failure from `FollowAction` as `FollowActionStatus::Failed` with a `FollowActionFailure` variant. Raw UniFFI carries the same terminal action fact, and both wrappers project the matching typed failure.
