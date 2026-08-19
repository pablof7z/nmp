# Optional Nostr content and UI building blocks

- **Date:** 2026-07-12
- **Status:** Ratified architecture for issue #75, amended by #561 and corrected
  by #879. Shared parsing and exact locator decoding are implemented without
  engine ownership or acquisition planning; Swift components own optional
  app-resolved observation, while Kotlin currently projects only the
  parser/locator contract. The first SwiftUI family and live iOS Gallery are
  implemented, as is the open-code registry/CLI. Broad Compose content UI,
  broader protocol families, and deeper cross-platform performance proof
  remain separately tracked work, not covered by this record.
- **Core boundary:** NMP Core remains the content-neutral live-query and
  write-intent engine. The content runtime and UI kits are optional consumers
  of its public API.
- **Evidence:** the old `nostr-multi-platform` `nmp-content`, component
  registry, installer, gallery, and three divergent `NostrInlineVideoPlayer`
  forks; shadcn's open-code distribution model; Bits UI's headless primitive
  model; SwiftUI and Compose's native composition and state-lifetime
  conventions.

## 1. Protocol resources versus controlled visuals

"State flows down; actions flow up" does not mean every protocol fact becomes
an app-owned Boolean and callback. For a reusable, correctness-sensitive
semantic transaction—especially a destructive whole-value replacement—the
pattern applied is that the optional protocol module exposes the live resource
and typed action through NMP's public facade, and native UI renders that state
and forwards intent.

NIP-02 is the first proof. `NMPFollowing` (`Packages/NMP/Sources/NMP/Following.swift`)
projects the active account's canonical kind:3 relationship and source-scoped
readiness; `NMPEngine.follow`/`unfollow` preserve the exact list and publish
under an atomic base precondition; `NMPFollowButton`
(`Packages/NMP/Sources/NMPUI/FollowButton.swift`) owns only pixels,
accessibility, and confirmation animation. The button cannot accept an
`isFollowing` Boolean or reconstruct kind:3.

Presentation-only or not-yet-semantic interactions may still be controlled
components: the NIP-25 reaction visuals accept selected/count/action from the
host because their protocol resource is separately tracked and did not exist
at the time of this record. The distinction applied is ownership, not visual
similarity: reusable Nostr correctness lives in an optional NMP module;
product state and appearance stay app-owned.

## 2. Controlled relay identity and runtime evidence

Relay presentation is a controlled visual boundary over two already-public,
separate facts. The caller invokes the engine-owned one-shot NIP-11 API and
passes its latest result as fresh, stale-last-good, loading, or unavailable. A
stale snapshot remains renderable while its freshness and last acquisition
error stay separate. Query-scoped `SourceStatus` is supplied independently;
the component does not fabricate URL-global connected, authenticated, healthy,
or reconnecting state.

The primitive owns no engine handle, HTTP client, timer, polling loop, cache,
or image loader. Advertised icon text may be exposed for app policy, but the
view accepts only an already-resolved SwiftUI `Image` or Compose `Painter`.
Issue #198 implements this family in SwiftUI and in a narrow optional
desktop-JVM Compose subproject (`RelayViews.kt`), confirmed as a real
cross-platform parity proof. That subproject is an API-parity proof, not an
Android/AAR qualification or broad Compose content-renderer implementation.
Kotlin otherwise projects only the parser/locator contract; no renderer files
exist there beyond this narrow relay family.

## 3. Options considered

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

## 4. Delivery record

Increments actually built, with their issue/PR evidence:

1. **Contract and parser boundary — built (#147, corrected by #567):** defined
   `ContentDocument`, stable occurrence identity, malformed fallback, and a
   parser with no engine or protocol-schema ownership.
2. **Locator proof — built (#567/#583, corrected by #879):** preserved
   `npub`/`nprofile`/`note`/`nevent`/`naddr` as exact distinct values with
   authored hints, created no demand or route policy, and proved exact
   Rust/FFI/Swift/Kotlin parity from one shared corpus.
3. **One platform component proof — built (#573):** SwiftUI document walking,
   literal zero-fetch components, component-owned visibility observations,
   outer event loading, actual-kind/purpose dispatch, and generic fallback
   with no app-root provider or shared session.
4. **Second platform parity proof — parser/locator built (#580/#583/#879),
   narrow relay family built (#198):** Kotlin consumes the same
   semantic/locator corpus, and controlled Compose relay primitives establish
   native construction (see §2). Broad Compose content UI does not exist.
5. **Hybrid distribution proof — built (#165 / PR #475):** installed one
   styled component whose linked primitives can update independently; local
   edits survive registry updates honestly.
6. **Gallery and performance gate — iOS proof built (#154):** the live
   Gallery, deterministic conformance states, screenshot-bearing UI tests, and
   a 72-row rapid-scroll nested-reference case
   (`apps/UIGallery/Sources/NMPUIGallery/StressGallery.swift`,
   `apps/UIGallery/Tests/NMPUIGalleryUITests/NMPUIGalleryUITests.swift:100`)
   exercise the production SwiftUI path and assert component handles/tasks
   return to baseline. Compose Gallery and deeper allocation/frame-time
   automation remain open.
7. **First protocol action component — built (#180, shipped as PR #184,
   "Ship NMP-owned NIP-02 follow action and SwiftUI button"):** NIP-02
   relationship state, guarded follow/unfollow, direct/FFI live-relay parity,
   and a SwiftUI button proved that reusable semantic action logic can remain
   in NMP while the optional view remains fully replaceable.

A kind-diverse renderer proof (a note plus materially different schemas such
as an article and a product/photo, including an app-defined fallback) was
never built under this record.

## 5. Honest remaining choices

The following were open at the time of this record and are not resolved by
it:

- final broad Compose content/package shape (the narrow relay proof used
  `com.nmp.ui` without freezing the rest of the ecosystem);
- exact default theme direction;
- the first protocol renderer set after a kind-diverse proof;
- default loader freshness/consent policy by presentation purpose;
- whether registry update uses an embedded merge library or shells out to Git;
- supported Compose platform/version matrix;
- governance for accepting third-party registry namespaces.

## 6. Prior art and historical evidence

- [shadcn/ui introduction](https://ui.shadcn.com/docs): open code,
  composition, flat-file distribution, and beautiful defaults.
- [shadcn CLI](https://ui.shadcn.com/docs/cli): selective add, view, diff,
  migration, and ejection capabilities.
- [Bits UI introduction](https://www.bits-ui.com/docs/introduction): linked
  headless primitives with stable APIs, accessibility, composability, and full
  styling control.
- [Compose state hoisting](https://developer.android.com/develop/ui/compose/state-hoisting):
  state stays near its lowest necessary owner and is exposed as immutable
  state plus events.
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
