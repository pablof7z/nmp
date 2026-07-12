# NMP UI building-blocks architecture exploration

Date: 2026-07-12
Project/context: NMP issue #75; successor to the old `nostr-multi-platform`
component registry and `nmp-content` system
Status: decided

## Core Question

- How should NMP offer optional, properly styled, extensible, multi-platform
  Nostr content-rendering building blocks so apps do not repeatedly implement
  parsing, reference resolution, kind-specific rendering, and nested embeds?
- How can those building blocks remain adoptable and app-owned without moving
  rendering or application architecture into NMP Core?

## Current Working Model

- The current v2 statement that rendering belongs to the app/UI framework is an
  ownership boundary, not an adequate reuse or distribution strategy.
- NMP Core remains the content-neutral live-query/write-intent engine.
- A separate optional content/UI ecosystem may own reusable document parsing,
  reference hydration, render-session lifecycle, kind dispatch, platform-native
  primitives, and styled default renderers while depending only on NMP's public
  surface.
- The likely product shape is layered: shared semantic/runtime services,
  platform-native primitives, composed default renderers, and an adoption model
  that permits linked updates, selective installation, ejection, or overrides.

## Observations

- The user considers content rendering a central product requirement, not a
  cosmetic convenience. Mixed content may contain `npub`, `naddr`, and `nevent`
  references whose resolved targets require different renderers, including
  NIP-23 articles, NIP-99 products, kind:1 notes, and kind:20 photos.
- The renderer space is open-ended. Every app rebuilding the same parser,
  resolver, nested-query lifecycle, and kind renderers is unacceptable.
- The user wants sensible, properly styled defaults built from bits-ui-style
  primitives, with shadcn-like ownership/customizability as inspiration rather
  than as a predetermined distribution mechanism.
- The old repo already contained substantial evidence: `nmp-content`, a
  cross-platform component registry, `NostrContentView`, `NostrKindRegistry`,
  component installation tooling, and multiple consumer forks.
- Issue #75 is open but explicitly unratified. The current
  `docs/design/ui-components-strategy.md` superseded its roadmap and currently
  recommends no component program; that position does not satisfy the user's
  clarified requirement.

## Constraints And Invariants

- UI/content packages are optional and are not part of NMP Core.
- NMP Core must remain blind to component catalogs, themes, renderers, and UI
  lifecycle.
- Apps must be able to adopt useful defaults selectively and take the result in
  materially different directions; the system cannot be take-it-or-leave-it.
- Protocol facts and network acquisition truth must remain consistent across
  platforms even though SwiftUI and Compose rendering code is native.
- Reference resolution must reuse NMP's canonical store, routing, query
  coalescing, evidence, and cancellation instead of creating a parallel cache
  or network stack.
- Recursive/nested rendering must be bounded, cancellable, cycle-safe, and
  explicit about unresolved or unavailable data.
- Stateful resource policy such as video playback must be replaceable.
- Distribution must support upstream fixes without forcing every app to accept
  every renderer or visual decision.

## Preferences

- Properly styled defaults, not bare protocol demos.
- Small composable primitives underneath higher-level components.
- App ownership comparable to shadcn/bits-ui: inspectable source, easy
  overrides, selective adoption, and no architectural lock-in.
- Preserve useful old-repo work where the mechanism remains sound.

## Assumptions

- SwiftUI and Compose are the first relevant native platforms; verify whether
  web is a design target or only a future compatibility constraint.
- The new NMP public live-query surface can support scoped child observations;
  verify exact lifecycle, evidence, deduplication, and FFI constraints.
- A hybrid linked-plus-source-owned distribution model may offer better update
  and ownership properties than either pure binary packages or pure copy-in;
  this is a hypothesis, not a decision.

## Open Questions

- What exact responsibilities belong in a shared content runtime versus each
  platform renderer package?
- Should nested reference resolution be exposed as an explicit render session,
  a public query-plan value, or platform-native child observation objects?
- How should kind dispatch remain extensible without becoming an engine-global
  content ontology?
- Which pieces should be linked dependencies, generated/copied source, or both?
- Can app-owned installed components receive safe upgrades without recreating
  the old silent-fork problem?
- How are themes, primitive slots, renderer overrides, navigation actions,
  resource loaders, and unknown kinds composed?
- What is the minimum useful default renderer set, and who owns protocol-specific
  typed values?
- What conformance/gallery proof is required across SwiftUI and Compose?

## Hypotheses

- A scoped `ContentSession` built above the public NMP API can own nested demand,
  deduplication keys, recursion budgets, and resolved-node state without making
  the engine UI-aware.
- Kind dispatch is legitimate in the optional renderer layer if it is local,
  composable, overrideable, and absent from NMP Core.
- A hybrid distribution may work best: linked semantic/runtime and primitive
  packages for shared fixes, plus source-installable composed components for
  app ownership and deep customization.
- Components should render a typed, observable document graph rather than raw
  callbacks or a pixel-oriented cross-platform render IR.

## Risks

- Recreating the old app-root host/provider and hidden engine-interior coupling.
- Replacing app duplication with an unmaintainable N-platform component matrix.
- Copy-in components silently diverging and missing upstream correctness fixes.
- A linked package becoming take-it-or-leave-it through monolithic APIs or
  styling assumptions.
- A central kind taxonomy claiming semantics owned by protocol modules.
- Nested embeds causing query explosions, cycles, resource leaks, or unbounded
  rendering work.
- Styling primitives becoming lowest-common-denominator abstractions across
  native platforms.

## Evidence Gathered

- `docs/design/ui-components-strategy.md`: old-system failure analysis,
  conditional R1-R6 gates, and current no-roadmap position.
- GitHub issue #75: proposed L1/L2/L3 model and shadcn-style copy-in direction.
- `../nostr-multi-platform/crates/nmp-content`: tokenizer, content tree, wire
  values, and embed projections.
- `../nostr-multi-platform/crates/nmp-component-registry`: SwiftUI/Compose
  registries, component manifests, native components, and installer inputs.
- `../nostr-multi-platform/crates/nmp-cli`: `nmp add component` installation and
  dependency closure behavior.
- Current NMP builder docs: native observation via `AsyncSequence`/`Flow`, app-
  controlled observation lifetime, and public rows/evidence boundary.
- shadcn official docs: open code, composition, flat-file distribution,
  beautiful defaults, selective add/view/diff, migrations, and ejection.
- Bits UI official docs: linked headless primitives, accessibility,
  composability, override-friendly defaults, and styling freedom.
- Apple/Android platform docs: source packages, native state lifetime, state
  hoisting, slots, and replaceable native design-system layers.

## Adjacent Checks

- Adjacent check: Which old component-system seams are inherently required by
  heterogeneous Nostr content, and which only compensated for the old engine?
  Finding: the content tree, recursion guard, claim/release, kind dispatch,
  dependency-aware installer, fixtures, and native overrides solve real content
  problems. `refs.event`/`refs.event.envelopes`, app-root host wiring, global
  registration, and interior projection dependencies were old-engine coupling.
  Implication: preserve the mechanisms but rebuild them as an optional content
  client over public live queries with explicit, scoped catalogs.
  Confidence: high.

- Adjacent check: Which distribution model best combines upstream fixes with
  app ownership?
  Finding: shadcn explicitly separates open-code styled compositions from its
  dependencies, while Bits UI demonstrates linked, composable headless
  primitives. The old NMP pure copy-in path preserved edits but stranded fixes;
  pure linked views would make deep product customization wrapper-heavy.
  Implication: link semantics/runtime/primitives and source-install styled
  compositions.
  Confidence: high.

## Alternatives Considered

- Pure linked UI kits: easy upgrades and centralized fixes, but risks monolithic
  take-it-or-leave-it APIs and styling pressure.
- Pure shadcn-style copy-in: strongest source ownership and editability, but
  old-repo evidence shows divergence and weak upstream fix propagation.
- Hybrid linked substrate plus source-owned composed views: potentially combines
  correctness updates with local ownership; complexity and upgrade semantics
  need proof.
- Headless-only runtime: solves parsing/resolution but still makes every app
  rebuild polished NIP-specific views.
- Cross-platform render IR: maximizes code sharing but risks becoming a UI
  framework and flattening native platform capabilities.

## Rejected Options

- Every app implements content rendering independently: rejected by the user's
  explicit product requirement and the open-ended protocol surface.
- Put renderer knowledge in NMP Core: rejected because it breaks the engine's
  content-neutral ownership boundary.
- Treat the current no-component roadmap as sufficient: rejected because it
  does not solve cross-app content reuse.

## Decisions Or Emerging Direction

- Explicit user direction: create a proper optional UI-building-blocks design;
  do not handwave content rendering into each app.
- Explicit user direction: defaults must be useful and styled, components must
  be built from extensible primitives, and apps must not face a binary adoption
  choice.
- Distribution is not yet decided; shadcn is inspiration, not a mandate.
- Selected after comparison: a hybrid distribution. Correctness-sensitive
  semantics, content sessions, and native primitives are linked/versioned;
  styled compositions are selectively source-installed and app-owned.
- Nested resolution is a reusable `ContentSession` responsibility over NMP's
  public live-query API, not work repeated in every app and not an NMP Core
  concern.
- A renderer catalog is legitimate and required in the optional UI layer. It is
  explicit, scoped, immutable after construction, overrideable, and never a
  global/core registry.
- Shared Rust code owns parsing, stable node identity, reference plans, budgets,
  and pure reducer semantics. Swift/Kotlin clients own native query tasks and
  view lifetime against the same fixture traces.

## Follow-Up Artifacts

- Replaced `docs/design/ui-components-strategy.md` with the promoted
  architecture direction.
- Reconcile GitHub issue #75 with the promoted design and split implementation
  children only after the architecture is ratified.
