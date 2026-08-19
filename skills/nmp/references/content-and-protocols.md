# Content and protocols

## Content packages

The base engine delivers raw event rows. Formatting and product policy remain app-owned.

Content support is parser-only: `nmp_content::parse_content` returns an immutable source-ranged document. Parsing is pure — it owns no protocol schema, renderer, component registry, query handle, cache, engine, or network client, and it holds nothing to close.

There is no typed profile/article resource layer and no bounded reference-session machinery: no `NMPContentClient`, no `NostrContentSession`, no `claim(referenceID:)`, and no session permit budget. To make a nested reference live, resolve its locator from the parsed document and open an ordinary query the app owns.

## NIP-02 following

The atomic follow/unfollow action lives in the `nmp-nip02` protocol crate. `nmp-nip02` depends on `nmp`, so it is not and cannot be re-exported through the facade — an `nmp -> nmp-nip02` edge is a package cycle cargo refuses. Supply `follow_capability()` to `Engine::new_with_capabilities` at construction, then `set_following(engine, &follow_writes(), target, change)` takes a decoded `PublicKey` and returns the ordinary `ReceiptStream`. Constructing the engine with that compiled capability means retained operations resume after restart without another follow/unfollow call. The action freezes the selected author and submits a versioned semantic operation immediately. NMP materializes it over the best current kind:3, or over NIP-02's complete empty first value when no source is known, and automatically reapplies it if newer relay truth arrives. It preserves unrelated contacts, hints, petnames, malformed/unowned tags, order, and content; the same durable operation and receipt own every successor generation.

`FollowWrites` is deliberately opaque. Only `set_following` can turn it into a write, and that action uses `Identity::Explicit(author)` so an account switch cannot retarget custody. There is no public current-row composer, operation-byte constructor, source id, or contributor state. Without that compiled capability at construction there is no operation door.

## NIP-22 comments

NIP-22 owns typed kind:1111 comments. Composition takes a single target and
never a separate root and parent: the root is read off the target's own rows,
so commenting on a thread root and replying to a deep comment are the same
call and cannot be got backwards. `nmp-nip22` exposes root-thread demand
(`comment_thread_demand`), strict decode (`decode_comment`, with an
exhaustive `CommentDecodeError`), and schema composition. Composition names
no author and reads no clock — the intent carries `Identity::Active` and no
`created_at`, so the engine resolves identity and stamps time at acceptance,
and two composes of the same comment are two valid events rather than one
repeated one.

The target is anything implementing `nmp_grammar::RootScope`:
`comment_intent(&target, content)` accepts a `CommentRoot` (event, address,
or NIP-73 external), a delivered `Row`, a signed `Event`, or a `Nip73` value
directly.

`nmp_nip22::comment_intent` returns the ordinary `WriteIntent` on the `Auto`
route, so durable author-outbox routing applies. Publish that value through
the generic engine `publish` door and observe its ordinary receipt. There is
no `engine.comment_intent`, `CommentIntent` wrapper, or take-once lifecycle.

## NIP-29 groups

A group can live on more than one relay at once, so the door is a scope named
once, narrowed to a group: `nmp_nip29::on(hosts)` returns a `RelayScope`
(fallible — `RelayScopeError::EmptyRelaySet` if the caller-supplied set is
empty), and `scope.group(groupId)` narrows it, keeping the same hosts. `on`
takes decoded `RelayUrl`s, never pasted strings. There is no single-host
constructor and no free single-host discovery function. `nmp_nip29::group(hosts,
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
set: `nmp_nip29::groups_whose_record_matches(Filter)` names the groups whose own
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

`nmp_nip29::all()` is the fifth spelling and the only one that is not a
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
`nmp_nip29::group(hosts, id)?.observe(&engine, records)`. Both refuse an empty
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

The signed-in account's remembered-group list is a separate kind:10009 value.
Read it with `nmp_nip29::current_account_group_list_demand` and parse a row
observationally with `nmp_nip29::parse_simple_groups_list_tolerant`; parser
output grants no mutation or routing authority. Typed mutations are
`nmp_nip29::add_group_to_list`, `remove_group_from_list`, `add_relay_in_use`,
and `remove_relay_in_use`. They return the ordinary receipt, work from an
empty first value, and durably reapply the same operation when a newer
kind:10009 source arrives. A saved group's identity is its group id plus
canonical host relay: adding it never rewrites an existing display name, and
removing it preserves same-id groups on other hosts, malformed/private data,
unrelated tags, content, and ordering. A host recorded inside kind:10009 is
data only; these writes use the selected author's ordinary outbox routing.

`nmp-nipc7` independently owns pure kind:9 chat, and its replies emit `e`, not
`q` — a kind:9 must not become a NIP-22 comment, because NIP-29 clients fetch
only kind 9. It composes schema only: no mentions, no notification `p` rows, no
NIP-29 `h`, no routing, and no content. When the message belongs to a group,
hand the composed draft to `Group::publish`, which is what supplies the `h`
row and the hosts.

## The one tagging door

Each composer lives as a free function in its own protocol crate —
`nmp_nipc7::chat`/`chat_reply`, `nmp_nip18::repost`, `nmp_nip25::react`,
`nmp_grammar::reply_to` (also reachable as `EventBuilder::reply_to`) — and
returns an `EventBuilder`, naming no author and reading no clock. Each
target-taking one takes the target `Event` plus an optional relay hint
(`sources: Option<RelayUrl>`) and nothing else — never a relationship,
marker, or author:

- `chat()` — top-level NIP-C7 kind:9.
- `chat_reply(target)` — kind:9 with the `e` row.
- `reply_to(target)` — the ordinary reply: NIP-10 for a text note, a NIP-22 comment for everything else.
- `repost(target, sources)` — NIP-18 picks kind:6 or kind:16 itself; a caller never states a kind.
- `react(target, sources, reaction)` — NIP-25, taking a closed `Reaction` (`Like`/`Dislike`/`emoji(...)`) rather than a raw content string, since NIP-25 assigns `+`, `-` and the empty string fixed meanings.

NIP-51 simple-groups lists (`nmp_nip29::current_account_group_list_demand`
plus `nmp_nip29::parse_simple_groups_list_tolerant`) live in the same
`nmp-nip29` crate. There is no blob-storage, media-upload or picture-event
door: Blossom, NIP-68 and the staged media composition seam were deleted
outright, so an app that needs one must add that owner from scratch. NIP-65
relay-list bootstrap composes `nmp_nip65::BootstrapRelayList::new(author,
bootstrap_relays, entries).into_write_intent()` and publishes it through the
generic `Engine::publish` door — `Engine::publish_relay_list_bootstrap` was
deleted as pure capability convenience over that same statement;
`nmp_nip65::relay_list_demand` is unaffected. There is no NIP-23 owner at
all — no crate, no `Article` type, no decode — so an article feature must add
that owner from scratch.

When implementing a protocol feature not already projected, do not assemble it from mechanism crates in app code. First determine whether it belongs in an opt-in protocol crate and whether it needs its own public API.

Relay connection/worker failure during NIP-02 observation is acquisition evidence, not `EngineError::ObservationUnavailable`; that error is reserved for an ordinary or windowed initial canonical-projection refusal after store degradation. Observation availability does not gate the semantic action. `set_following`'s only failures are `FollowActionFailure::SignedOut`, `EngineClosed`, and `PublishRefused { reason }`.
