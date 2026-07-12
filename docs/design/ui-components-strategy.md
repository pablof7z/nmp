# Optional Nostr content and UI building blocks

- **Date:** 2026-07-12
- **Status:** Architecture direction for issue #75, refined after the complete
  content-format, inline-composition, selection, and annotation requirements
  review recorded in the issue. This replaces the superseded "no component
  roadmap" recommendation. Implementation remains issue-first and separately
  sequenced.
- **Core boundary:** NMP Core remains the content-neutral live-query and
  write-intent engine. The content runtime and UI kits are optional consumers
  of its public API.
- **Evidence:** the old `nostr-multi-platform` `nmp-content`, component registry,
  installer, gallery, and three divergent `NostrInlineVideoPlayer` forks;
  shadcn's open-code distribution model; Bits UI's headless primitive model;
  SwiftUI and Compose's native composition and state-lifetime conventions;
  NIP-23 Markdown, NIP-54 Djot, NIP-84 highlights, and the concrete 29er-style
  channel preview that must refine a raw `nostr:npub...` into a compact mention.

## 1. Decision

NMP will have an optional, multi-platform content/UI ecosystem whose purpose is
to make correct Nostr content rendering reusable across applications.

The ecosystem is not part of NMP Core, but it is a real product surface. “The
app owns presentation” means the app has final authority over its product
experience. It does **not** mean every app must independently rebuild Nostr URI
parsing, reference acquisition, recursive embed lifecycle, kind dispatch,
accessibility, media layout, article rendering, product rendering, or all the
other machinery needed to render an open protocol well.

The selected distribution model is deliberately hybrid:

1. **Linked, versioned substrate** for correctness-sensitive semantics,
   reference-session lifecycle, and low-level native primitives. Fixes to
   parsing, cancellation, accessibility, and resolution propagate normally.
2. **Source-installable, styled compositions** for opinionated cards, readers,
   product views, and blocks. An app receives readable native source, may edit
   it without permission, and may selectively install only what it uses.
3. **Explicit app composition** for renderer selection, navigation, resource
   policy, and product-specific overrides. No global import side effects and no
   engine-owned renderer registry.

This is the shadcn/Bits split adapted to native applications: stable primitives
underneath, beautiful open-code compositions on top.

## 2. The problem is a content runtime, not a `Text` view

This content is ordinary on Nostr:

```text
hello nostr:npub1... read my article nostr:naddr1...
and buy my product nostr:naddr1...
and see this note nostr:nevent1...
and this photo nostr:nevent1...
```

Rendering it requires a coordinated system:

- parse plaintext and protocol-selected markup such as Markdown and Djot
  without corrupting source text;
- recognize NIP-21/NIP-27 entities, hashtags, links, custom emoji, invoices,
  media, code spans, and protocol extensions;
- decode `npub`, `nprofile`, `note`, `nevent`, and `naddr` correctly;
- turn each reference into the right live demand, including relay hints,
  authors, coordinates, and source authority;
- render cached content immediately while acquisition continues;
- resolve profiles and event references without a second cache or one network
  stack per view;
- preserve loading, invalid, unavailable, shortfall, deletion, replacement,
  and provenance facts;
- dispatch resolved events to kind/schema-specific renderers;
- recurse when an embedded event contains more references;
- prevent cycles, runaway depth, query explosions, and scroll-time resource
  leaks;
- support rich native renderers for notes, profiles, articles, products,
  photos, highlights, and future protocol modules;
- retain source-to-rendered-text mapping for native selection, copy/paste,
  accessibility ranges, and inline annotation anchoring;
- render related NIP-84 highlights conservatively and turn native text
  selection back into a valid highlight write intent;
- allow the app to replace navigation, theming, media, wallet, purchase, and
  other product policy.

That complexity must be solved once as reusable infrastructure and exercised
continuously across platforms.

## 3. Product goals

An application should be able to start with something equivalent to:

```swift
NostrContent(event: row.event, content: contentClient)
```

```kotlin
NostrContent(event = row.event, content = contentClient)
```

and receive a useful, styled, accessible default renderer with live mentions
and embeds. It should then be able to move progressively, without a rewrite,
through these levels of ownership:

1. change theme tokens;
2. replace a primitive slot such as media, profile name, or embed chrome;
3. replace one renderer such as the NIP-23 article card;
4. install and edit the source of a composed renderer;
5. add a renderer for an app-defined or newly standardized kind;
6. replace the whole top-level content view while retaining the parser and
   reference runtime;
7. replace every optional layer and consume NMP Core directly.

The same gradient applies to reading interactions: an app may use ordinary
non-selectable content, add native text selection, attach a NIP-84 annotation
layer for an explicitly chosen trust set, replace the visual highlight mark, or
replace the complete reader while retaining source-faithful parsing and normal
NMP live queries/write intents.

The adoption path is therefore a gradient, not take-it-or-leave-it.

## 4. Ownership and dependency direction

```text
Application
  screens · navigation · product policy · local overrides
      │
      ├── source-installed styled components and blocks
      │     channel preview · note card · article/wiki reader
      │     product card/view · photo view · highlight interactions
      │
      ├── linked native primitive kit
      │     content · inline runs · embed · profile · media
      │     selection · decorations · article · product primitives
      │
      ├── optional protocol/content modules
      │     NIP-23 · NIP-54 · NIP-84 · NIP-99 · app-defined schemas
      │
      └── linked content client/runtime
            parser · semantic document · source map · render session
            reference-to-demand lowering · annotation anchoring
                         │
                         ▼ public API only
                    NMP Core
              live queries · store · routing · evidence
```

The dependency arrow points downward only:

- NMP Core has no renderer catalog, component manifest, theme, view type, or UI
  lifecycle concept.
- The optional content runtime depends on NMP's supported public facade.
- Native primitive kits depend on the content runtime and native UI framework.
- Styled components depend on primitives and are copied into the app.
- The app may replace or bypass any optional layer.

Repository placement does not define the boundary; the dependency graph does.
The recommended physical split is:

- this repository owns the optional shared content semantics and platform
  content-client packages because they must track the governed NMP facade and
  FFI contract closely;
- a sibling `nmp-ui` repository owns native primitive packages, source
  registries, design tokens, galleries, and styled components while consuming
  released NMP public artifacts only.

This keeps the engine repository from becoming a component catalog while still
making content resolution a supported, tested part of the NMP developer story.

## 5. Layer A — shared content semantics

Layer A is optional, linked, cross-platform semantic code. Its output describes
what the content **is**, never how pixels are arranged.

### 5.1 Content document

The old `ContentTree` is the correct starting point, with its accidental policy
removed. The document vocabulary should cover protocol or source-text facts,
including document structure needed by Markdown, Djot, selection, and
accessibility without describing platform pixels:

```text
ContentDocument
  identity · revision · syntax · originalSource
  blocks[]

Block
  Paragraph(inlines)
  Heading(level, inlines)
  Quote(blocks)
  List(items)
  CodeBlock(language?, source)
  Table · DefinitionList · Footnote · ThematicBreak
  Unsupported(originalSource, reason)

Inline
  Text · Emphasis · Strong · CodeSpan · Link
  Mention(NostrEntity.Profile | NostrEntity.Pubkey)
  EventReference(NostrEntity.Event | EventId | Coordinate)
  WikiReference(normalizedIdentifier)
  Hashtag · Url(syntacticMediaHint?) · CustomEmoji · Invoice
  Unsupported(originalSource, reason)
```

Rules:

- Every node has stable identity within a document revision and retains its
  source range and original text.
- Parsing is deterministic, side-effect free, and separately testable.
- The document exposes a deterministic canonical textual projection and a
  bidirectional mapping between semantic/rendered textual leaves and source
  ranges. Resolved display text such as `@Alice` never destroys the stable
  `nostr:nprofile...` source identity.
- Syntax is explicit and selected by the protocol/app module that owns the
  schema. Content diagnostics identify which adapter selected it. Generic
  content code never guesses from a global kind table:

  | Schema | Content syntax |
  |---|---|
  | readable short text such as kind:1 | plaintext with NIP-21/NIP-27 augmentation |
  | kind:30023 / NIP-23 | Markdown; authored raw HTML is not supported |
  | kind:30818 / current NIP-54 | Djot with NIP-21 targets and NIP-54 wikilinks |
  | legacy or app-defined schema | explicit adapter selected by its owner |

  Real legacy kind:30818 AsciiDoc support, if observed and required, belongs in
  an explicit legacy adapter or deliberate app policy. It must not silently
  replace the current Djot contract or rely on an unexplained global heuristic.
- A URL may carry a conservative syntactic hint derived from its source or
  extension. MIME confirmation, media grouping, gallery layout, truncation,
  “2h ago,” and display-name fallback are presentation and do not enter the
  shared document vocabulary.
- New token variants require cross-platform fixtures and a fallback rendering
  rule.
- Invalid or unsupported entities fall back to their original source text.

The source map is foundational rather than a later highlighting add-on. Native
selection, copy/paste, annotation anchoring, and accessibility all depend on
mapping visual ranges back to stable semantic/source ranges.

### 5.2 Reference identities and placement

The content model keeps three identities separate:

```text
OccurrenceId
  one position in one document

ReferenceTargetKey
  the normalized profile, immutable event, or address

ReferenceAcquisitionKey
  target selection + source authority + access context
```

Five mentions of one profile are five occurrences and normally one semantic
target. Two equal targets under different source authorities or access contexts
are distinct acquisitions with distinct evidence; a target-only resource key
must never alias them. NMP Core remains responsible for coalescing compatible
underlying acquisition work.

Each occurrence also retains source placement such as inline, standalone block,
or markup link. Placement says what the author wrote. Rendering purpose—channel
preview, embed, feed card, search result, or detail reader—is supplied later by
the native UI call site and never participates in parsing or NMP demand
identity.

### 5.3 Protocol-owned typed values

Each opt-in protocol module owns the exact semantic values for its schema:

- a NIP-23 module may decode an article title, summary, image, published time,
  and Markdown body;
- a NIP-54 module owns Djot wiki content, identifier normalization, wikilinks,
  redirects, and exact wiki-event semantics;
- a NIP-84 module owns typed highlights, source/context/attribution tags,
  quote-highlight semantics, conservative anchoring, and highlight drafts;
- a NIP-99 module may decode a classified/product value and its protocol fields;
- a photo module may decode the exact photo event schema it owns;
- an app-defined module may expose its own typed value.

There is no central Rust enum that must be extended for every renderable Nostr
kind. Modules expose typed decoders/adapters, and the optional renderer catalog
associates those adapters with native views. Raw-event fallback remains
permanent so unknown kinds never render as blank space.

For an app-defined kind, invariant-bearing or cross-platform schema decoding
belongs in an app-owned Rust/protocol module and crosses through an app-owned
projection seam. Native code owns presentation. A raw-event fallback may expose
unknown data, but it must not become a reason for Swift and Kotlin to implement
the same protocol semantics independently. The projection/code-generation DX
is an early architecture proof: adding a valid app kind such as `61234` must
not require a central NMP Core or generated FFI-kind enum edit.

### 5.4 What is shared across platforms

The parser, entity decoding, stable node identity, recursion-budget rules, and
protocol decoders should be shared Rust semantics projected to native values.
Canonical text/source mapping and annotation-anchor state transitions are also
shared semantics. SwiftUI and Compose must not independently reinterpret the
same NIP fields or attach an ambiguous annotation differently.

## 6. Layer B — the content client and render session

Layer B is the missing piece in both the current no-component direction and the
old “pure renderer” rule. It prevents every application from rebuilding nested
query orchestration.

### 6.1 Content client

An app creates one optional content client from an existing engine:

```swift
let contentClient = NMPContentClient(engine: engine)
```

```kotlin
val contentClient = NmpContentClient(engine)
```

This is not a second engine. It owns no event database, sockets, relay routing,
or global account state. It uses public live queries and relies on NMP Core for
canonical rows, query sharing, routing, provenance, and evidence.

Environment/`CompositionLocal` injection may be offered as convenience, but is
never required. Every component must have an explicit initializer accepting the
content client or an already-created session. A bare preview/test can use a
scripted session without constructing an engine.

### 6.2 Render session

A `ContentSession` is scoped to one root document or rendered event. It exposes
an observable latest-state `ContentSnapshot`:

```text
ContentSnapshot
  document
  resources: ReferenceAcquisitionKey -> ResourceState
  nodes: OccurrenceId -> NodeState
  revision
  activeReferenceCount
  shortfalls
```

Reference node states are explicit:

```text
idle
loading(cachedRow?)
resolved(row, typedValue?, evidence)
unavailable(evidence)
shortfall(reason, evidence)
invalid(originalText, reason)
collapsed(depth | cycle | budget)
```

The resource table shares latest query state where the acquisition identity is
actually equal. Per-occurrence node state remains separate because placement,
render path, cycle state, and hydration can differ even when two occurrences
point at the same target.

The runtime never translates scoped evidence into “globally missing.” A failed
relay hint or EOSE is a fact about that acquisition path, not proof that the
referenced event does not exist.

### 6.3 Reference lowering

The session converts Nostr entities into ordinary public NMP demands:

- `npub` / `nprofile` -> current kind:0 metadata for that author;
- `note` -> exact event-id selection;
- `nevent` -> exact event-id selection; optional author/kind values are
  validation/routing hints rather than extra match constraints, and relay hints
  inform acquisition;
- `naddr` -> exact address selection: kind + author + `d` identifier, retaining
  replaceable-event semantics;
- nested references -> the same process under a descended render context.

This lowering is the complete fetching boundary. The content layer never
selects the winning event, interprets an out-of-order replacement, maintains a
parallel cache, or compensates for a missing negative delta. A live `naddr`
query may first expose a cached winner and later a newer winner; deletion,
expiry, replacement, and retraction likewise change the ordinary NMP query
snapshot. The content session forwards that newest snapshot and the renderer
updates. Any defect in those facts is fixed and falsified in NMP Core, not
reimplemented above it.

Every resolved result is validated against the normalized target before it is
projected: event ids must match `note`/`nevent`; an address result must match
kind, author, and `d`; profile metadata must be a valid kind:0 event by the
requested pubkey. Embedded NIP-19 author/kind/relay values remain hints where
the protocol defines them as hints.

Acquisition uses a configurable `ReferenceAcquisitionPolicy`. The sensible
default is:

1. expose matching cached rows immediately;
2. use explicit relay hints when present;
3. use author outboxes when an author is known and the target is author-owned;
4. otherwise use the configured public/indexer authority;
5. retain each path's evidence rather than merging it into a false global
   success/failure flag.

Access/privacy restrictions inherited from the rendering context are never
silently widened. Source authority is target-specific rather than blindly
copied from the parent: a profile mentioned inside content acquired from a
pinned group host may still belong on the profile author's outboxes, subject to
the enclosing access restrictions and explicit app policy. Diagnostics retain
whether hints were used, rejected, or unavailable.

One logical reference may therefore own multiple ordinary live-query handles.
NMP Core still coalesces compatible wire work and preserves distinct contextual
evidence.

### 6.4 Claim/release and visibility

Parsing a document must not eagerly fetch an unbounded number of embeds. Each
session receives a closed hydration policy, with exact spelling provisional:

```text
none
profilesOnly
standaloneReferences
allWithinBudget
explicit(visible occurrences or acquisition keys)
```

The policy always includes explicit depth, distinct-target, active-target,
node, and concurrent-acquisition caps. `explicit` permits viewport-driven
hydration without placing a UI callback or closure in the NMP demand path.
Every valid but unhydrated reference remains visible as source text or a compact
link with a typed budget/policy state.

Within that policy, each resolvable node supports idempotent claim/release:

- a native primitive claims a node when it becomes render-relevant;
- the last release tears down its child query after a small configurable grace
  period to avoid scroll thrash;
- identical targets within or across sessions share NMP's underlying demand and
  cache even though their render paths remain distinct;
- a session has explicit caps for active references, total resolved nodes,
  recursion depth, and concurrent acquisitions;
- exceeding a cap yields a visible collapsed/shortfall state, never silent
  truncation.

The old claim/release concept was sound. Its defect was requiring apps to wire
`refs.event`, `refs.event.envelopes`, and an app-root host. The new client owns
that orchestration directly over the public live-query API.

An implementation may maintain a private keyed set of ordinary NMP query
handles and apply closed-set diffs as visibility changes. That is a
content-session implementation option, not a new public engine noun or a new
cache: NMP already deduplicates compatible live demands and withdraws work when
the last query handle drops. A generic public collection API is extracted only
if later independent consumers and measurements prove it is useful beyond this
session boundary.

### 6.5 Lifetime

- A Swift session follows ARC/task cancellation and emits through an
  `AsyncSequence` or `@Observable` adapter on the correct actor.
- A Kotlin session follows coroutine/`Flow` cancellation.
- Dropping a session releases all claims and query handles deterministically.
- A native view may own a session for convenience; apps may also create and
  retain sessions in their existing state architecture.
- No app-wide NMP `ViewModel`, reducer, provider, or navigation container is
  required.

### 6.6 Shared versus native implementation

The split must be explicit so “cross-platform” does not become either duplicated
protocol logic or a hidden UI framework:

- optional Rust content code owns parsing, stable node ids, entity-to-reference
  plans, canonical/source mapping, recursion/budget rules, annotation anchor
  transitions, and pure snapshot-reducer semantics;
- Swift and Kotlin content clients own their native `NMPQuery`/`Flow` tasks,
  visibility claims, actor/coroutine lifetime, and projection into observable
  platform state;
- the same fixture traces drive the pure reducer and both native clients;
- query sharing, cache, routing, and evidence remain in NMP Core.

This avoids forcing a foreign Rust-owned view lifecycle across FFI while keeping
the protocol and state-transition semantics shared. If a later prototype proves
a Rust-owned session object can share the supported engine facade without a
second FFI component or host callback scaffold, it may replace the duplicated
native orchestration. The public `ContentSession` contract does not depend on
that internal choice.

## 7. Layer C — native headless primitives

Layer C is a linked, versioned SwiftUI and Compose primitive kit. Pixel code is
implemented natively on each platform; API concepts and conformance fixtures
remain aligned.

The primitives are analogous to Bits UI: behaviorally complete, accessible,
composable, minimally styled, and useful underneath many visual compositions.

Candidate primitive families include:

- `Content.Root`, `Content.Text`, `Content.Link`, `Content.Hashtag`;
- `Document.Selectable`, `Document.SelectionAction`,
  `Document.DecorationLayer`;
- `Mention.Root`, `Mention.Avatar`, `Mention.Name`;
- `Embed.Root`, `Embed.Loading`, `Embed.Unavailable`, `Embed.Content`;
- `Event.Root`, `Event.Author`, `Event.Timestamp`, `Event.Body`, `Event.Actions`;
- `Article.Root`, `Article.Hero`, `Article.Title`, `Article.Byline`,
  `Article.Body`;
- `Wiki.Root`, `Wiki.Link`, `Wiki.Body`;
- `Highlight.Mark`, `Highlight.Gutter`, `Highlight.Popover`;
- `Product.Root`, `Product.Media`, `Product.Title`, `Product.Price`,
  `Product.Actions`;
- `Media.Grid`, `Media.Image`, `Media.VideoSlot`, `Media.Overflow`;
- `Profile.Avatar`, `Profile.Name`, `Profile.Nip05`, `Profile.About`;
- `UnknownEvent` and `RawEventDisclosure`.

These names are illustrative, not frozen API.

### 7.1 Primitive contract

- State flows down; typed actions flow up.
- Primitives consume a content session, node, or typed protocol value. They do
  not parse raw events independently.
- SwiftUI uses generic `@ViewBuilder` slots; Compose uses composable lambdas and
  standard `Modifier` conventions.
- Inline slots do not imply one heavyweight child view per token. Each platform
  may use native attributed runs, annotations, attachments, or inline
  composables so wrapping, selection, copy/paste, accessibility, and identity
  remain correct. Rich event cards normally become block embeds; compact
  surfaces may render the same occurrence as an inline title/link.
- Simple element-local state may stay local. Business/product state remains
  app-controlled.
- Accessibility labels, focus behavior, dynamic type/font scaling, reduced
  motion, RTL, and input semantics are part of primitive correctness.
- Theme values use native environment/`CompositionLocal` patterns and can be
  overridden for any subtree.
- Primitives do not navigate. They emit typed actions such as open profile,
  open event, open URL, open hashtag, inspect relay evidence, or invoke a
  protocol-specific action supplied by its renderer.
- Selectable document primitives own native gestures, handles, focus, menus,
  and accessibility actions, then emit a source-mapped `ContentSelection`.
  They do not decide highlight protocol tags or publish directly.

### 7.2 Resource-owning slots

Resource lifecycle is extensible by construction:

- image loading;
- video/audio playback;
- link-preview HTTP work;
- invoice/wallet interaction;
- product purchase/contact flows;
- file download or Blossom upload.

A styled component may supply a conservative default, but the primitive accepts
a replacement. No canonical renderer constructs a new media player during body
evaluation, and no application must fork an article card merely to change video
autoplay policy.

## 8. Layer D — renderer catalog

Heterogeneous content requires dispatch. Refusing to name that requirement does
not remove it; it merely makes every app write a private switch statement.

The renderer catalog belongs in the optional native UI layer, never in NMP Core.

### 8.1 Catalog properties

- Explicitly constructed and immutable after construction.
- Scoped per app, screen, or subtree.
- No registration through import side effects.
- No process-global mutable singleton.
- Deterministic duplicate handling: adding a second renderer for the same key is
  an error unless the caller explicitly uses an override operation.
- Permanent unknown-kind fallback.
- Separate token renderers from resolved-event renderers.
- Renderer packages may register exact kinds/schema adapters owned by their
  protocol module; apps may register their own kinds.

Lookup precedence is deterministic:

```text
direct call-site slot
  -> nearest screen/subtree catalog
  -> application catalog
  -> standard renderer
  -> generic/raw fallback
```

The shared occurrence supplies source placement. The native call site may also
supply a rendering purpose such as channel preview, embedded, card, or detail.
Purpose is presentation policy: it remains native, does not cross FFI as a
central Rust enum, and never affects parsing, content-session identity, or NMP
demand. Separate compositions are preferred over a giant mode enum.

Illustrative composition:

```swift
let catalog = NostrRendererCatalog.standard
    .install(Nip23ArticleRenderer())
    .install(Nip99ProductRenderer())
    .overriding(kind: appKind, with: AppRecordRenderer())
```

```kotlin
val catalog = NostrRendererCatalog.standard()
    .install(Nip23ArticleRenderer())
    .install(Nip99ProductRenderer())
    .override(appKind, AppRecordRenderer())
```

Passing a different catalog to a notification subtree can select compact
renderers without changing the rest of the app.

### 8.2 Renderer input contract

A renderer receives enough bounded, policy-free input to render every state
without opening another engine observation:

```text
RendererInput
  original occurrence and URI
  normalized target
  current ResourceState and compact scoped evidence
  source placement and native rendering purpose
  render path / depth / cycle state
  typed application actions
```

Actions cover presentation-side intent such as open profile/event/URL, copy a
reference, inspect evidence, or invoke an app-supplied protocol action. They run
after data delivery and cannot affect NMP filtering, routing, cache admission,
winner selection, or acquisition policy. Renderers never fetch, navigate
through a required framework, or manufacture hard-coded routes.

### 8.3 Dispatch flow

```text
resolved canonical row
  -> protocol adapter, if installed
  -> exact native renderer, if installed
  -> generic event renderer
  -> raw-event disclosure as final fallback
```

The catalog chooses presentation after delivery. It cannot influence demand,
relay admission, store winner selection, or protocol validation.

## 9. Selection and NIP-84 annotation layers

A NIP-84 highlight is a separate kind:9802 related event that refers to source
content; it is not embedded inside the source event. A reader explicitly opts
into an annotation layer and chooses whose highlights matter—only the current
user, followed people, a curated trust set, everybody, or none. That
trust/moderation choice is app product policy.

The read flow composes ordinary NMP queries:

```text
source event/address live query
  -> protocol decoder + source-mapped ContentDocument

app-selected kind:9802 live query
  -> typed NIP-84 highlights
  -> shared conservative anchor resolver
  -> ContentDecoration states

ContentDocument + decorations
  -> native selectable document primitive
```

The typed NIP-84 value preserves highlighted content, `e`/`a`/`r` source
references, optional context, attributed `p` tags and roles, and
quote-highlight/comment semantics. External-URL highlights may be represented,
but they never imply hidden HTTP acquisition; preview/fetching remains an
explicit app capability and security policy.

NIP-84 defines quote text and optional context but no character offsets. Blind
substring replacement is therefore forbidden. Anchoring returns an explicit
state:

```text
resolved(document revision, semantic ranges)
ambiguous(candidate ranges)
orphaned(reason)
invalid(reason)
```

An `e` source identifies an exact immutable version. An `a` source identifies a
logical address whose NMP winner may change. When an addressable document
changes, annotations may re-anchor against the new revision; a quote that no
longer resolves uniquely becomes ambiguous/orphaned rather than silently moving
to the wrong text. A standard highlight builder may include both an exact `e`
version and an `a` logical address, but that source-tag policy requires explicit
protocol review rather than being invented inside a renderer.

Highlights are non-destructive decorations over the document, not mutations of
the authoritative AST. The decoration layer supports live additions/removals,
multiple authors, and overlapping ranges. Shared semantics own ranges and
anchor states; native primitives own range composition/accessibility; the app
owns colors, marks, avatars, popovers, visibility, and interaction.

Creating a highlight is the reverse flow:

```text
native text selection
  -> ContentSelection
  -> NIP-84 draft builder
  -> ordinary NMP write intent
```

`ContentSelection` retains the source event/address, document revision,
semantic leaf ranges, canonical quote, current display text, and surrounding
canonical context. Native primitives own selection mechanics. The content
runtime maps visual ranges to source/semantic ranges. The NIP-84 module builds
the protocol event. The app owns confirmation/comment/product flow. NMP owns
acceptance, signing, persistence, routing, retry, and outcomes.

Canonical and display text remain distinct: a user may see `@Alice` where the
source contains `nostr:nprofile...`, and Markdown/Djot markup may not appear in
rendered text. Dynamic display names must never corrupt the stable quote.
Selection across unsupported non-text embeds or structural boundaries produces
an explicit unsupported state rather than a malformed highlight.

The same contract applies to selectable plaintext notes, NIP-23 Markdown
articles, and NIP-54 Djot wiki documents. Format-specific parsers feed one
source-map/selection/decoration interface; platform renderers do not reparse
them independently.

## 10. Layer E — styled open-code components

Layer E supplies the useful defaults the primitive layer intentionally does not.
These are polished native components and blocks distributed as source into the
application.

Examples:

- minimal inline Nostr text;
- full mixed-content view;
- compact channel/message preview;
- compact and standard note cards;
- quote/event embed;
- NIP-23 article card and reader;
- NIP-54 wiki link and selectable reader;
- NIP-84 highlight mark, annotation popover/gutter, and selection action;
- NIP-99 product card and detail view;
- photo card/gallery;
- profile chip/card;
- media grid and lightbox composition;
- unknown-event fallback;
- thread block;
- composer pieces where a protocol module supplies the write semantics.

### 10.1 Concrete channel-preview canary

A channel-list row must render a compact projection of the semantic document,
not truncate an opaque raw string before parsing:

```text
nostr:npub1x3h90...
  -> Mention(pubkey)
  -> ordinary NMP profile live query
  -> compact inline renderer: @29er-next
```

Cached metadata renders immediately when available; an unresolved mention uses
an intelligible shortened identifier and refines reactively when metadata
arrives. The channel subtree may supply a name-only renderer while the full
message uses an avatar/name treatment. No channel-specific parser, profile
cache, relay query, or update observer is permitted. Compact truncation operates
over semantic nodes so enrichment is not discarded before rendering.

### 10.2 Component contract

Every component must:

- look sensible immediately under the default theme;
- be built from Layer C primitives rather than a monolith;
- declare source files, linked dependencies, registry dependencies, supported
  platform versions, and renderer keys;
- expose important subviews as slots or small replaceable source files;
- emit actions instead of owning navigation or product flows;
- compile in a one-screen bare host with a scripted content session;
- include previews/examples and accessibility metadata;
- use only released public NMP/content/UI APIs.

The app owns the installed source. Documentation may recommend extension seams,
but it may never prohibit the app from editing its own component.

## 11. Distribution and update design

### 11.1 Why neither extreme works

**Pure linked UI kit:** propagates fixes, but a large opinionated package becomes
take-it-or-leave-it, encourages wrapper stacks, and makes deep visual changes
fight a foreign API.

**Pure source copy-in:** maximizes ownership, but correctness and accessibility
fixes stop propagating. The old AVPlayer forks prove that a canonical bug and
three divergent app copies can coexist indefinitely.

The hybrid boundary puts code on the side matching its change character:

| Linked and versioned | Source-installed and app-owned |
|---|---|
| parsing and entity decoding | styled cards and blocks |
| reference/session lifecycle | visual composition |
| protocol semantic adapters | app-specific renderer catalog assembly |
| accessibility/behavior primitives | local theme presets and product chrome |
| stable fallback behavior | opinionated resource-policy choices |

### 11.2 Registry

A standalone `nmp-ui` registry and CLI distributes native source items. The
shape borrows from shadcn/jsrepo and the old NMP registry, with these commands as
the intended capability set rather than frozen spelling:

```text
nmp-ui search
nmp-ui view swiftui/article-card
nmp-ui add swiftui/article-card
nmp-ui diff swiftui/article-card
nmp-ui update swiftui/article-card
nmp-ui migrate <migration>
```

Requirements:

- install only selected items and their declared dependency closure;
- support SwiftUI and Compose as independent native targets;
- allow namespaced third-party registries;
- preview and diff before writing;
- perform safe path validation;
- never silently overwrite local edits;
- record the exact upstream base for every installed file;
- use a three-way merge for updates when possible;
- leave unresolved files and the component version honestly conflicted rather
  than advancing the lock as though the update succeeded;
- support explicit overwrite or re-install only with user intent;
- keep custom renderer files separate from upstream-owned installed files when
  the app wants easy fast-forward updates;
- make every generated mutation reviewable as an ordinary source diff.

The old registry's dependency graph, roles, hashes, conflict preservation, and
fixtures are reusable foundations. Its update behavior must be corrected: a
conflicted file cannot retain its old base hash while the component-level lock
advances and pretends the new version is installed.

### 11.3 Ejection and long-term ownership

Source-installed components are already ejected at the visual layer: they are
ordinary app files from day one. An app may also vendor or fork a linked
primitive/runtime package, but doing so is an explicit dependency decision with
the understood cost of leaving the upstream fix stream.

## 12. Styling and customization

Each platform has a native default theme with semantic tokens, not hard-coded
brand colors:

```text
colors · typography · spacing · shapes · borders · elevation/material
content density · embed chrome · media aspect policy
```

Rules:

- Defaults are carefully designed and production-usable.
- Tokens can inherit from the app's native design system.
- Any subtree can override theme values.
- Component parameters/slots override theme defaults when local control is
  needed.
- Themes never cross FFI and never affect engine or content-session identity.
- Compact/standard/reader layouts are separate compositions, not giant mode
  enums with dozens of unrelated switches.

## 13. Extensibility examples

### 13.1 Replace one article card

An app installs the standard article component, edits the source to match its
brand, and explicitly overrides only the article renderer. Parsing, reference
resolution, Markdown semantics, mentions, nested embeds, and evidence continue
to receive linked fixes.

### 13.2 Add an app-defined kind

The app defines a typed decoder for its own event schema and a native renderer,
then adds one explicit catalog entry. Shared/invariant-bearing decoding lives in
an app-owned Rust/protocol module; native packages render its typed projection.
No NMP Core switch statement, central ontology/FFI-kind change,
registry-server approval, or hand-edit of generated binding internals is
required. A single-platform app may always use the generic raw fallback, but
cross-platform semantics are not duplicated inside renderers.

### 13.3 Change media policy

The app keeps the standard note/article compositions but supplies a lazy,
tap-to-play video slot. Another app supplies eager playback. Neither forks the
content parser or reference runtime.

### 13.4 Use only the headless runtime

An app with a radically different design can ignore all styled components and
walk `ContentSnapshot` using its own views. It still avoids rebuilding parsing,
entity lowering, nested query lifetime, and cycle/budget handling.

## 14. Failure and fallback rules

- Invalid token: render the original source text.
- Unknown event kind: generic event card, then optional raw disclosure.
- Missing renderer dependency: use the generic fallback; never blank space.
- Cached row available while acquisition runs: render cached content and retain
  scoped evidence.
- Relay failure/EOSE: expose scoped state; never claim global absence.
- Deleted/expired/replaced row: update through the ordinary live-query path.
- Reference cycle: render a collapsed link/card explaining the cycle boundary.
- Depth or active-reference budget reached: render a collapsed continuation.
- Hydration policy excludes a valid reference: render its original text or a
  compact link with a typed policy/budget state.
- Slow consumer: deliver the latest complete content snapshot.
- Media loader failure: preserve layout and expose a retry/open-externally slot.
- Protocol decoder failure: fall back to the generic raw event renderer.
- Unsupported content syntax: preserve and expose the original source.
- Highlight quote resolves more than once: expose an ambiguous annotation; do
  not choose an occurrence silently.
- Highlight quote no longer matches a document revision: expose an orphaned
  annotation rather than moving it.
- Selection crosses an unsupported non-text/structural boundary: return an
  explicit unsupported selection state and do not build a malformed draft.

## 15. Security and privacy

- Nostr URI parsing rejects secret-key entities and malformed payloads.
- Resolved `note`/`nevent`, `naddr`, and profile events are validated against
  the exact normalized id, coordinate, or author target before projection.
- Relay hints pass through NMP's relay-admission policy; a renderer cannot turn
  an arbitrary `.onion`, loopback, private, or otherwise disallowed URL into a
  transport connection.
- HTTP link previews and media loads are separate capabilities with explicit
  SSRF, redirect, MIME, size, and privacy policy; they are not implied by Nostr
  event acquisition.
- Embedded private/decrypted content must not be inserted into a public shared
  cache, rendered outside its authorized access context, or leaked through a
  public annotation query.
- Rendered Markdown, Djot, HTML-like input, and raw fallback never execute
  arbitrary script or unsafe markup.
- External-URL highlights and source links never imply background HTTP work;
  HTTP remains an explicit app capability with its own admission policy.
- Recursion, node, byte, media, annotation, and concurrent-acquisition budgets
  are enforced before work is scheduled.

## 16. Verification strategy

The UI ecosystem needs stronger proof than “the package compiles.”

### Shared semantic fixtures

- one corpus covering plaintext, NIP-23 Markdown, NIP-54 Djot, every supported
  Nostr entity, Unicode, malformed inputs, code/literal spans, wikilinks,
  custom emoji, invoices, overlapping matches, and nested references;
- identical expected semantic documents across Rust, Swift, and Kotlin;
- stable node ids, original ranges, canonical text, and bidirectional
  source/rendered-leaf mappings across every syntax;
- canonical/display-text fixtures for dynamic mentions and markup;
- protocol-specific typed-value fixtures owned by each module.

### Reference boundary and render-session falsifiers

- `npub`/`nprofile`, `note`/`nevent`, and `naddr` lower to the exact supported
  public NMP demand and acquisition context;
- author/kind/relay hints are validated and never accidentally become extra
  selection constraints;
- two visible occurrences with the same acquisition identity share session
  state and compatible underlying NMP demand;
- equal targets under different source/access contexts never alias evidence;
- release of one claimant does not close another claimant's work;
- final release closes after the configured grace window;
- replacing the latest input query snapshot A with B updates every mounted
  occurrence without local winner/cache logic;
- relay hints, target-specific sources, and enclosing access restrictions retain
  distinct evidence;
- a self-reference and a multi-event cycle collapse deterministically;
- every hydration mode plus depth, target, node, and concurrency caps produces
  explicit states while leaving valid unhydrated references visible;
- scroll churn does not grow active queries or tasks without bound;
- dropping the session deterministically releases all content-derived handles;
- one real-engine integration proves the complete reference-to-query-to-view
  path and forwards NMP winner/retraction changes.

Cache hits, out-of-order replacement selection, deletion, expiry, and negative
deltas remain NMP Core falsifiers. The content package proves only correct
lowering, lifetime, bounded projection, and transparent forwarding; it must
never duplicate those engine algorithms to make its own tests pass.

### Selection and annotation falsifiers

- exact, repeated, contextual, ambiguous, orphaned, overlapping, and
  version-changed NIP-84 quotes have deterministic fixtures;
- live highlight additions/removals update decorations without reparsing or
  mutating the document truth;
- selection maps to stable canonical quote/context across plaintext, Markdown,
  and Djot;
- dynamic mention names cannot silently replace canonical source identity;
- unsupported selections fail explicitly before a draft exists;
- selection -> NIP-84 typed draft -> ordinary NMP write intent uses supported
  public surfaces and preserves source/attribution semantics.

### Platform conformance

- every primitive and source component compiles in a bare sample app;
- scripted previews cover loading, resolved, unavailable, shortfall, unknown,
  cycle, budget, selection, resolved/ambiguous/orphaned annotation, and unknown
  syntax states;
- accessibility, dynamic type/font scale, dark mode, RTL, reduced motion, and
  keyboard/focus behavior are exercised where applicable;
- screenshot/golden tests cover the default styled components;
- a 29er-like channel row turns a raw profile URI into a compact reactive
  mention without app-owned parsing/cache/query code;
- direct, nearest-subtree, app, standard, and fallback renderer precedence is
  proven, including native placement-versus-purpose behavior;
- an app-owned typed decoder plus native renderer for kind `61234` requires no
  NMP Core or central generated FFI-kind edit;
- SwiftUI and Compose galleries consume only released public surfaces and the
  same semantic/session/selection transition corpus;
- a minimal structurally different adapter, preferably TUI, proves the shared
  contract is not accidentally mobile/pixel-shaped;
- a real-engine mock-relay test proves the complete reference-to-query-to-view
  path on both platforms;
- real-device rapid scrolling returns memory, tasks, claims, and NMP handles to
  baseline, delivers at no more than 60 Hz, and exhibits no main-thread jank.

### Independent DX trial

A developer who did not design the API must be able to render the mixed-content
corpus, replace only a channel-preview mention, add app kind `61234`, replace
article video policy, select/publish a highlight, and edit/update an installed
component without architectural coaching. Needing to change Core, introduce an
app cache/query layer, copy a parser, extend a central kind enum, or hand-edit
generated bindings is a design failure.

### Registry falsifiers

- dependency closure is deterministic;
- add/diff/view are stable and safe;
- unmodified files fast-forward;
- edited files three-way merge or remain honestly conflicted;
- a conflicted update never advances the installed version falsely;
- deleting a local file is not silently undone;
- third-party namespaces cannot escape the app root;
- installed source remains buildable after supported migrations.

## 17. Options considered

### A. No official content/UI ecosystem

Rejected. It preserves a clean core by exporting an unreasonable amount of
open-protocol complexity into every application.

### B. Headless semantics only

Rejected as the complete answer. It helps parsing and resolution but still
requires every app to rebuild polished article, product, photo, note, profile,
and unknown-kind renderers.

### C. Pure linked UI packages

Rejected as the only distribution. It is good for primitives and correctness,
but poor as the sole home of opinionated, deeply customized product views.

### D. Pure shadcn-style copy-in

Rejected as the only distribution. It maximizes control but strands parser,
lifecycle, accessibility, and resource fixes in app forks.

### E. Cross-platform render IR

Rejected. Sharing pixel/layout nodes across SwiftUI and Compose creates a UI
framework, constrains native capabilities, and still requires platform
interpreters. Shared semantics stop before pixels.

### F. Hybrid linked substrate plus source-installed styled compositions

Selected. It places update-sensitive correctness in dependencies and
product-sensitive composition in app-owned source.

## 18. Required boundaries

These are the structural gates for implementation:

1. **Core blindness:** no NMP Core dependency on content/UI packages.
2. **Public-surface-only:** content/UI packages build against released NMP
   facade products, not engine-interior crates or projection names.
3. **No parallel truth:** content runtime owns no event store or transport.
4. **No app-root requirement:** explicit initializers work without a provider;
   environment injection is convenience only.
5. **Scoped resolution:** nested demand belongs to observable, cancellable,
   bounded content sessions.
6. **Explicit catalog:** no import-time or process-global renderer mutation.
7. **Native composition:** slots and child builders are normal SwiftUI/Compose
   constructs, not a cross-platform render IR.
8. **Open-code top layer:** styled compositions can be installed selectively and
   edited freely.
9. **Propagating substrate fixes:** semantics, lifecycle, and primitives remain
   versioned dependencies by default.
10. **Permanent fallbacks:** unknown, invalid, unavailable, and shortfall states
    always render intelligibly.
11. **Kind diversity:** conformance includes unrelated kinds and app-defined
    schemas; kind:1 is not the architecture's privileged center.
12. **Honest updates:** registry tooling never overwrites edits or reports a
    conflicted component as current.
13. **Source fidelity:** syntax, original ranges, canonical text, and
    rendered/source mapping survive every optional layer.
14. **Identity separation:** occurrence, semantic target, and acquisition
    identity remain distinct; source/access evidence never aliases by target
    alone.
15. **Native interaction boundary:** selection mechanics and decoration pixels
    are native; selection mapping, annotation anchoring, and NIP-84 semantics
    are shared/protocol-owned; publication is an ordinary NMP write intent.
16. **Engine truth ownership:** the content runtime forwards normal NMP query
    snapshots and never owns cache, routing, winner, replacement, deletion, or
    retry truth.
17. **App-kind seam:** a custom typed kind does not require a Core ontology or
    central generated FFI-kind edit.

## 19. Sequencing

Implementation should be split into issue-backed vertical proofs:

1. **Semantic contract and corpus:** define `ContentDocument`, explicit syntax
   adapters, stable node identity, canonical text/source maps, and malformed
   fallback across plaintext, Markdown, and Djot.
2. **Reference adapter/session proof:** lower NIP-19 targets through ordinary
   public NMP queries with identity separation, claim/release, hydration,
   evidence, cycle, budget, and newest-snapshot forwarding falsifiers.
3. **Selection/decoration contract:** define `ContentSelection`,
   `ContentDecoration`, anchor states, and deterministic transition traces
   before platform implementation freezes the wrong text model.
4. **SwiftUI vertical proof:** mixed content, a compact 29er-like preview,
   selectable document, scripted/real sessions, and no app-root provider.
5. **Compose parity proof:** equivalent native primitives over the same
   semantic, session, selection, and decoration corpus.
6. **App-defined kind DX proof:** app-owned typed decoder/projection plus native
   renderer for kind `61234`, without a central Core/FFI-kind edit.
7. **NIP-84 vertical proof:** related-event query, conservative anchoring, live
   decorations, native selection, typed draft builder, and NMP write intent.
8. **Hybrid distribution proof:** install one styled component whose linked
   primitives can update independently; prove local edits survive registry
   updates honestly.
9. **Kind-diverse renderer proof:** ship a note plus materially different
   article, product/photo, wiki, unknown, and app-defined schemas.
10. **Gallery, accessibility, security, and performance gate:** native
    galleries, a TUI portability probe, device measurements, adversarial
    content, registry falsifiers, and the independent DX trial.

No broad renderer breadth should be built before steps 1-8 prove the hard
architecture seams. Once the foundation is proven, renderer breadth is an
ongoing product program rather than a one-time milestone.

## 20. Honest remaining choices

The architecture above settles ownership and distribution boundaries. The
following still require implementation issues or owner selection:

- final package/repository names;
- one normalized extensible semantic AST versus syntax-specific ASTs behind a
  common source-map/inline protocol;
- exact canonical-text rules for markup and dynamically resolved entities;
- the conservative NIP-84 quote/context anchoring algorithm and source-tag
  policy for addressable content (`e`, `a`, or both);
- the app-owned typed decoder/projection mechanism that avoids central FFI
  edits;
- whether observed legacy kind:30818 content justifies an AsciiDoc adapter;
- whether SwiftUI or Compose is the first vertical proof;
- exact default theme direction;
- the first protocol renderer set after the kind-diverse proof;
- the default reference-acquisition fallback timings and budgets;
- whether registry update uses an embedded merge library or shells out to Git;
- supported platform/version matrix, including whether web/TUI begin as
  production targets or conformance probes;
- whether a session-private keyed query table ever earns extraction as a public
  collection helper after independent use and performance evidence;
- governance for accepting third-party registry namespaces.

Those choices do not reopen the central decision: reusable Nostr rendering is
an optional NMP ecosystem responsibility, with linked correctness primitives
and app-owned styled compositions.

## 21. Epic completion contract

Issue #75 is not complete merely because packages compile or a gallery exists.
It is complete when supported applications can:

- render mixed open-ended Nostr content with polished native defaults;
- show resolved inline mentions in compact and full contexts using different
  scoped renderers;
- render plaintext, NIP-23 Markdown, and NIP-54 Djot through shared,
  source-faithful semantics;
- customize or replace any visual layer without rebuilding NMP acquisition or
  content parsing;
- install, edit, and honestly update app-owned styled source components;
- add an app-defined typed kind without changing NMP Core;
- display NIP-84 highlights conservatively inline;
- select rendered text and publish a valid highlight through an ordinary NMP
  write intent;
- remain accessible, bounded, native-feeling, and intelligible for unknown,
  invalid, unavailable, ambiguous, orphaned, and budget states across supported
  platforms;
- prove those claims in real consumers and running galleries, not only fixtures
  or compilation.

## 22. Prior art and historical evidence

- [Issue #75 complete requirements proposal](https://github.com/pablof7z/nmp/issues/75#issuecomment-4952262969):
  event-truth boundary, syntax/source maps, inline overrides, channel preview,
  NIP-84 selection/annotations, falsification scope, and epic done-when.
- [NIP-19](https://github.com/nostr-protocol/nips/blob/master/19.md),
  [NIP-21](https://github.com/nostr-protocol/nips/blob/master/21.md), and
  [NIP-27](https://github.com/nostr-protocol/nips/blob/master/27.md): encoded
  targets, `nostr:` URIs, inline references, and reader-controlled augmentation.
- [NIP-23](https://github.com/nostr-protocol/nips/blob/master/23.md):
  long-form Markdown semantics.
- [NIP-54](https://github.com/nostr-protocol/nips/blob/master/54.md): Djot wiki
  content and wikilinks.
- [NIP-84](https://github.com/nostr-protocol/nips/blob/master/84.md): highlight
  source, attribution, context, and quote-highlight semantics.

- [shadcn/ui introduction](https://ui.shadcn.com/docs): open code,
  composition, flat-file distribution, and beautiful defaults.
- [shadcn CLI](https://ui.shadcn.com/docs/cli): selective add, view, diff,
  migration, and ejection capabilities.
- [Bits UI introduction](https://www.bits-ui.com/docs/introduction): linked
  headless primitives with stable APIs, accessibility, composability, and full
  styling control.
- [Compose state hoisting](https://developer.android.com/develop/ui/compose/state-hoisting):
  state stays near its lowest necessary owner and is exposed as immutable state
  plus events.
- [Compose custom design systems](https://developer.android.com/develop/ui/compose/designsystems/custom):
  native themes and components can be extended, partially replaced, or fully
  replaced using public APIs.
- [Swift packages](https://developer.apple.com/documentation/xcode/swift-packages):
  source packages are normal reusable dependencies and can be overridden with
  local packages when deeper ownership is needed.
- [Old NMP content crate](https://github.com/pablof7z/nostr-multi-platform/tree/master/crates/nmp-content),
  [component registry](https://github.com/pablof7z/nostr-multi-platform/tree/master/crates/nmp-component-registry),
  and [component installer](https://github.com/pablof7z/nostr-multi-platform/tree/master/crates/nmp-cli):
  evidence for the tokenizer, recursion guard, claim/release, kind dispatch,
  source registry, dependency closure, fixtures, and update failure modes this
  design refines rather than discards.
