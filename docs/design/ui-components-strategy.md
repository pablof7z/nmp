# Optional Nostr content and UI building blocks

- **Date:** 2026-07-12
- **Status:** Ratified architecture for issue #75. The shared content engine
  is implemented by #147. The first SwiftUI family and live iOS Gallery are
  implemented by #154; Compose, the open-code registry/CLI, and broader
  protocol families remain separately tracked work.
- **Core boundary:** NMP Core remains the content-neutral live-query and
  write-intent engine. The content runtime and UI kits are optional consumers
  of its public API.
- **Evidence:** the old `nostr-multi-platform` `nmp-content`, component registry,
  installer, gallery, and three divergent `NostrInlineVideoPlayer` forks;
  shadcn's open-code distribution model; Bits UI's headless primitive model;
  SwiftUI and Compose's native composition and state-lifetime conventions.

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

- tokenize plaintext and Markdown without corrupting source text;
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
- allow the app to replace navigation, theming, media, wallet, purchase, and
  other product policy.

That complexity must be solved once as reusable infrastructure and exercised
continuously across platforms.

## 3. Product goals

An application can start with:

```swift
let session = contentClient.session(content: row.content)
NostrContent(session: session, renderers: .standard)
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

The adoption path is therefore a gradient, not take-it-or-leave-it.

## 4. Ownership and dependency direction

```text
Application
  screens · navigation · product policy · local overrides
      │
      ├── source-installed styled components and blocks
      │     note card · article card/reader · product card/view · photo view
      │
      ├── linked native primitive kit
      │     content · embed · profile · media · article · product primitives
      │
      └── linked content client/runtime
            parser · entity decoder · render session · reference acquisition
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
The implemented physical split is:

- this repository owns the optional shared content semantics and platform
  content-client packages because they must track the governed NMP facade and
  FFI contract closely;
- this repository also owns first-party native primitive packages, the source
  registry, galleries, and styled component sources as separately optional
  products above the public content boundary. The CLI is named `nmp-ui`; it is
  not a subcommand or hidden mode of the engine CLI.

Co-location keeps the components continuously tested against their public
substrate. Dependency direction—not repository geography—keeps NMP Core from
becoming a component framework.

## 5. Layer A — shared content semantics

Layer A is optional, linked, cross-platform semantic code. Its output describes
what the content **is**, never how pixels are arranged.

### 5.1 Content document

The old `ContentTree` is the correct starting point, with its accidental policy
removed. The document vocabulary should cover protocol or source-text facts:

```text
ContentDocument
  Text
  Mention(NostrEntity.Profile | NostrEntity.Pubkey)
  EventReference(NostrEntity.Event | EventId | Coordinate)
  Hashtag
  Url(syntacticMediaHint?)
  CustomEmoji
  Invoice
  MarkdownBlock(children)
  InvalidReference(originalText, reason)
```

Rules:

- Tokens retain stable source ranges and original text.
- Parsing is deterministic, side-effect free, and separately testable.
- Plaintext versus Markdown is explicit. A protocol module may select a mode
  for a schema it owns; generic core never guesses from a global kind table.
- A URL may carry a conservative syntactic hint derived from its source or
  extension. MIME confirmation, media grouping, gallery layout, truncation,
  “2h ago,” and display-name fallback are presentation and do not enter the
  shared document vocabulary.
- New token variants require cross-platform fixtures and a fallback rendering
  rule.
- Invalid or unsupported entities fall back to their original source text.

### 5.2 Protocol-owned typed values

Each opt-in protocol module owns the exact semantic values for its schema:

- a NIP-23 module may decode an article title, summary, image, published time,
  and Markdown body;
- a NIP-99 module may decode a classified/product value and its protocol fields;
- a photo module may decode the exact photo event schema it owns;
- an app-defined module may expose its own typed value.

There is no central Rust enum that must be extended for every renderable Nostr
kind. Modules expose typed decoders/adapters, and the optional renderer catalog
associates those adapters with native views. Raw-event fallback remains
permanent so unknown kinds never render as blank space.

### 5.3 What is shared across platforms

The parser, entity decoding, stable node identity, recursion-budget rules, and
protocol decoders should be shared Rust semantics projected to native values.
SwiftUI and Compose must not independently reinterpret the same NIP fields.

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
  nodes: NodeId -> NodeState
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

Acquisition uses a configurable `ReferenceAcquisitionPolicy`. The sensible
default is:

1. expose matching cached rows immediately;
2. use explicit relay hints when present;
3. use author outboxes when an author is known and the target is author-owned;
4. otherwise use the configured public/indexer authority;
5. retain each path's evidence rather than merging it into a false global
   success/failure flag.

One logical reference may therefore own multiple ordinary live-query handles.
NMP Core still coalesces compatible wire work and preserves distinct contextual
evidence.

### 6.4 Claim/release and visibility

Parsing a document must not eagerly fetch an unbounded number of embeds. Each
resolvable node supports idempotent claim/release:

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
  plans, recursion/budget rules, and pure snapshot-reducer semantics;
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
- `Mention.Root`, `Mention.Avatar`, `Mention.Name`;
- `Embed.Root`, `Embed.Loading`, `Embed.Unavailable`, `Embed.Content`;
- `Event.Root`, `Event.Author`, `Event.Timestamp`, `Event.Body`, `Event.Actions`;
- `Article.Root`, `Article.Hero`, `Article.Title`, `Article.Byline`,
  `Article.Body`;
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
- Simple element-local state may stay local. Business/product state remains
  app-controlled.
- Accessibility labels, focus behavior, dynamic type/font scaling, reduced
  motion, RTL, and input semantics are part of primitive correctness.
- Theme values use native environment/`CompositionLocal` patterns and can be
  overridden for any subtree.
- Primitives do not navigate. They emit typed actions such as open profile,
  open event, open URL, open hashtag, inspect relay evidence, or invoke a
  protocol-specific action supplied by its renderer.

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

Swift composition:

```swift
let renderers = NostrContentRenderers.standard
    .event(kind: appKind, purpose: .embedded) { input in
        AppRecordCard(event: input.event)
    }
```

```kotlin
val catalog = NostrRendererCatalog.standard()
    .install(Nip23ArticleRenderer())
    .install(Nip99ProductRenderer())
    .override(appKind, AppRecordRenderer())
```

Passing a different catalog to a notification subtree can select compact
renderers without changing the rest of the app.

### 8.2 Dispatch flow

```text
resolved canonical row
  -> protocol adapter, if installed
  -> exact native renderer, if installed
  -> generic event renderer
  -> raw-event disclosure as final fallback
```

The catalog chooses presentation after delivery. It cannot influence demand,
relay admission, store winner selection, or protocol validation.

## 9. Layer E — styled open-code components

Layer E supplies the useful defaults the primitive layer intentionally does not.
These are polished native components and blocks distributed as source into the
application.

Examples:

- minimal inline Nostr text;
- full mixed-content view;
- compact and standard note cards;
- quote/event embed;
- NIP-23 article card and reader;
- NIP-99 product card and detail view;
- photo card/gallery;
- profile chip/card;
- media grid and lightbox composition;
- unknown-event fallback;
- thread block;
- composer pieces where a protocol module supplies the write semantics.

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

## 10. Distribution and update design

### 10.1 Why neither extreme works

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

### 10.2 Registry

A standalone `nmp-ui` CLI distributes native source items. The
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

### 10.3 Ejection and long-term ownership

Source-installed components are already ejected at the visual layer: they are
ordinary app files from day one. An app may also vendor or fork a linked
primitive/runtime package, but doing so is an explicit dependency decision with
the understood cost of leaving the upstream fix stream.

## 11. Styling and customization

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

## 12. Extensibility examples

### 12.1 Replace one article card

An app installs the standard article component, edits the source to match its
brand, and explicitly overrides only the article renderer. Parsing, reference
resolution, Markdown semantics, mentions, nested embeds, and evidence continue
to receive linked fixes.

### 12.2 Add an app-defined kind

The app defines a typed decoder for its own event schema and a native renderer,
then adds one explicit catalog entry. No NMP Core switch statement, central
ontology change, or registry-server approval is required.

### 12.3 Change media policy

The app keeps the standard note/article compositions but supplies a lazy,
tap-to-play video slot. Another app supplies eager playback. Neither forks the
content parser or reference runtime.

### 12.4 Use only the headless runtime

An app with a radically different design can ignore all styled components and
walk `ContentSnapshot` using its own views. It still avoids rebuilding parsing,
entity lowering, nested query lifetime, and cycle/budget handling.

## 13. Failure and fallback rules

- Invalid token: render the original source text.
- Unknown event kind: generic event card, then optional raw disclosure.
- Missing renderer dependency: use the generic fallback; never blank space.
- Cached row available while acquisition runs: render cached content and retain
  scoped evidence.
- Relay failure/EOSE: expose scoped state; never claim global absence.
- Deleted/expired/replaced row: update through the ordinary live-query path.
- Reference cycle: render a collapsed link/card explaining the cycle boundary.
- Depth or active-reference budget reached: render a collapsed continuation.
- Slow consumer: deliver the latest complete content snapshot.
- Media loader failure: preserve layout and expose a retry/open-externally slot.
- Protocol decoder failure: fall back to the generic raw event renderer.

## 14. Security and privacy

- Nostr URI parsing rejects secret-key entities and malformed payloads.
- Relay hints pass through NMP's relay-admission policy; a renderer cannot turn
  an arbitrary `.onion`, loopback, private, or otherwise disallowed URL into a
  transport connection.
- HTTP link previews and media loads are separate capabilities with explicit
  SSRF, redirect, MIME, size, and privacy policy; they are not implied by Nostr
  event acquisition.
- Embedded private/decrypted content must not be inserted into a public shared
  cache or rendered outside its authorized access context.
- Rendered Markdown/HTML never executes arbitrary script or unsafe markup.
- Recursion, node, byte, media, and concurrent-acquisition budgets are enforced
  before work is scheduled.

## 15. Verification strategy

The UI ecosystem needs stronger proof than “the package compiles.”

### Shared semantic fixtures

- one corpus covering plaintext, Markdown, every supported Nostr entity,
  Unicode, malformed inputs, custom emoji, invoices, overlapping matches,
  source-range preservation, and nested references;
- identical expected semantic documents across Rust, Swift, and Kotlin;
- protocol-specific typed-value fixtures owned by each module.

### Render-session falsifiers

- two visible references to the same target share underlying demand;
- release of one claimant does not close another claimant's work;
- final release closes after the configured grace window;
- `naddr` selects the correct current replaceable winner;
- relay hints and fallback sources retain distinct evidence;
- a self-reference and a multi-event cycle collapse deterministically;
- depth, node, and concurrency budgets produce explicit states;
- scroll churn does not grow active queries or tasks without bound;
- cached rows render before live acquisition completes;
- deletion/replacement/retraction updates mounted embeds.

### Platform conformance

- every primitive and source component compiles in a bare sample app;
- scripted previews cover loading, resolved, unavailable, shortfall, unknown,
  cycle, and budget states;
- accessibility, dynamic type/font scale, dark mode, RTL, reduced motion, and
  keyboard/focus behavior are exercised where applicable;
- screenshot/golden tests cover the default styled components;
- SwiftUI and Compose galleries consume only released public surfaces;
- a real-engine mock-relay test proves the complete reference-to-query-to-view
  path on both platforms.

### Registry falsifiers

- dependency closure is deterministic;
- add/diff/view are stable and safe;
- unmodified files fast-forward;
- edited files three-way merge or remain honestly conflicted;
- a conflicted update never advances the installed version falsely;
- deleting a local file is not silently undone;
- third-party namespaces cannot escape the app root;
- installed source remains buildable after supported migrations.

## 16. Options considered

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

## 17. Required boundaries

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

## 18. Sequencing

Implementation should be split into issue-backed vertical proofs:

1. **Contract and fixtures — built (#147):** define `ContentDocument`, stable node identity,
   malformed fallback, and the shared cross-platform corpus.
2. **Reference-session proof — built (#147):** resolve `npub`/`nevent`/`naddr` through public
   NMP queries with claim/release, evidence, cycle, and budget falsifiers.
3. **One platform primitive proof — built (#154):** SwiftUI content/embed primitives consuming
   scripted and real sessions, with no app-root provider.
4. **Second platform parity proof:** equivalent Compose primitives and the same
   fixture/session contract.
5. **Hybrid distribution proof:** install one styled component whose linked
   primitives can update independently; prove local edits survive registry
   updates honestly.
6. **Kind-diverse renderer proof:** ship a note plus at least two materially
   different schemas such as an article and a product/photo, including an
   app-defined fallback/override.
7. **Gallery and performance gate — iOS proof built (#154):** the live Gallery,
   deterministic conformance states, screenshot-bearing UI tests, and a 72-row
   rapid-scroll nested-reference case now exercise the production SwiftUI path
   and assert visible claims return to a bounded window. Compose Gallery and
   deeper allocation/frame-time automation remain open.

No broad catalog should be built before steps 1-5 prove the architecture. Once
the foundation is proven, renderer breadth is an ongoing product program rather
than a one-time milestone.

## 19. Honest remaining choices

The architecture above settles ownership and distribution boundaries. The
following still require implementation issues or owner selection:

- final Compose package names;
- exact default theme direction;
- the first protocol renderer set after the kind-diverse proof;
- the default reference-acquisition fallback timings and budgets;
- whether registry update uses an embedded merge library or shells out to Git;
- supported Compose platform/version matrix;
- governance for accepting third-party registry namespaces.

Those choices do not reopen the central decision: reusable Nostr rendering is
an optional NMP ecosystem responsibility, with linked correctness primitives
and app-owned styled compositions.

## 20. Prior art and historical evidence

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
