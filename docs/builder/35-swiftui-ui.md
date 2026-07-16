# SwiftUI content and components

`NMPUI` is the optional native UI product above parser-only `NMPContent`. A
`NostrContent` view walks an immutable document and invokes app-selected
components. The walk itself does not attach a modifier, open an observation, or
imply network work.

```swift
import NMP
import NMPContent
import NMPUI

let engine = try NMPEngine(
    config: .init(indexerRelays: [
        "wss://purplepag.es",
        "wss://relay.primal.net",
    ])
)
let document = parseNostrContent(
    "hello nostr:npub1... read nostr:naddr1..."
)
let observations = NMPReferenceObservationFactory.live(engine: engine)
```

Render with the standard components:

```swift
NostrContent(
    document: document,
    observationFactory: observations,
    renderers: .standard
)
```

Passing a factory only makes observation available. `NostrContent` never calls
it directly. The selected profile component or outer event loader decides
whether to use it.

## Prove the zero-acquisition path first

The same document can be rendered literally even when a live factory is in
scope:

```swift
NostrContent(
    document: document,
    observationFactory: observations,
    renderers: .literalReferences
)
```

Both `npub` and event references remain authored text. No handle and no relay
work is created. This is not an optimization of the standard loader; it is a
different component choice.

There is deliberately no `NostrInlineContent`, `NostrBlockContent`, public
`ListBlock`, or public `CodeBlock`. `NostrContent` owns the semantic document
walk and lays text plus arbitrary native views into flow. Apps choose
components, not parser plumbing.

## The component hierarchy

```text
NostrContent
  document walk
    profile reference -> app-selected profile component
      literal component: no observation
      NMPStandardProfileMention: owns optional kind:0 observation
    event/address reference -> app-selected outer loader
      literal/consent/cache loader: app policy
      NMPDefaultEventLoader: owns optional canonical/helper observations
        NMPResolvedEventDispatcher
          actual event.kind + purpose -> registered renderer
          otherwise -> generic event fallback
```

The outer loader and resolved-event table are independent extension points. A
loader can change acquisition policy without changing how kind:1 or kind:30023
renders. A renderer only receives a validated acquired `Row`; it does not
fetch.

An authored `nevent` kind hint never selects the renderer. Dispatch uses
`input.event.kind` plus `NostrContentPurpose`. Unknown kinds always reach the
generic event component instead of blank space.

## Component-owned visibility

`NMPVisibleReferenceObservation` is per component. It asks
`referenceDemandPlan(for:)` for the canonical/helper demands and owns a fresh
independent handle for every observation it opens.

```swift
struct MyResolvingComponent: View {
    @StateObject private var observation: NMPVisibleReferenceObservation

    init(target: NostrReferenceTarget, factory: NMPReferenceObservationFactory) {
        _observation = StateObject(
            wrappedValue: NMPVisibleReferenceObservation(
                target: target,
                factory: factory
            )
        )
    }

    var body: some View {
        MyResolvedOrFallbackView(batch: observation.canonical)
            .observeWhileVisible(observation)
    }
}
```

`observeWhileVisible` is optional reusable behavior, not framework policy:

- appearing opens only this component's handles;
- leaving the scroll-visible region releases all of those handles;
- the last canonical/helper batches remain renderable while hidden, so return
  does not flicker empty;
- scroll thrash cannot accumulate handles or iteration tasks;
- custom components may instead observe unconditionally or never observe.

Equal components still own independent handles. Core may coalesce their equal
demands into one wire subscription, but releasing one component cannot release
the other component's interest.

## Choose acquisition policy in the component

The observation factory is an injectable seam around ordinary `engine.observe`.
The standard factory opens the supplied `NMPDemand` unchanged. A custom loader
can wrap or replace it to choose the merged per-handle freshness policy from
#565:

```swift
let cacheOnly = NMPReferenceObservationFactory { demand, receive in
    var demand = demand
    demand.freshness = .cacheOnly
    return try observations.observe(demand, receive: receive)
}
```

A consent loader can render from `cacheOnly`, show "Fetch preview?" when the
canonical batch is empty, and open a separate `.live` observation only after
the user agrees. A feed mention can choose `.maxAge(seconds: 14_400)` while a
profile detail component chooses `.live`. Parser output, target identity, and
the resolved-event renderer table stay unchanged.

Freshness is not a second cache or timer. `CacheOnly` contributes no wire work;
an unsatisfied `MaxAge` opens ordinary cache-then-live work once. Evidence and
shortfalls remain on the exact handles the component chose.

## Replace profile and event decisions separately

Literal profile references with otherwise-standard event behavior:

```swift
let renderers = NostrContentRenderers.standard
    .profileReference { input in
        NMPReferenceLiteral(original: input.occurrence.original)
    }
```

A replaceable outer event loader:

```swift
let renderers = NostrContentRenderers.standard
    .eventLoader { input in
        ConsentBeforeNetworkEventLoader(input: input)
    }
```

The custom loader may eventually delegate its acquired row without copying the
kind switch:

```swift
NMPResolvedEventDispatcher(
    reference: input,
    event: acquiredRow,
    context: descendedContext
)
```

Register an app-only kind independently:

```swift
let renderers = renderers
    .event(kind: 12_938, purpose: .embedded, layout: .block) { input in
        MyPrivateRecordCard(event: input.event)
    }
    .fallbackEvent { input in
        MyRawEventCard(event: input.event)
    }
```

No Rust enum, FFI union, app-root provider, or process-global registry changes.

## Immutable recursion context

`NostrContentRenderContext` carries only ancestor target keys, current depth,
and maximum depth. `descending(into:)` produces the next value or returns
`nil` for a cycle/depth stop. There is no mutable document coordinator and no
active/resolved reference count budget.

The default event loader descends before acquisition and passes the resulting
context into `NMPResolvedEventDispatcher`. Nested `NostrContent` receives that
same immutable context.

## Identity primitives

`NMPAvatar` and `NMPName` are presentation leaves used by mentions, bylines,
event chrome, and user cards:

```swift
NMPAvatar(pubkey: pubkey, profile: profile, size: 44)
NMPName(pubkey: pubkey, profile: profile)
```

Before a profile protocol owner supplies a decoded value, Avatar uses a
deterministic pubkey-derived fallback and Name uses an abbreviated pubkey. The
standard profile mention owns its kind:0 observation when a factory is
supplied; the content package does not decode kind:0.

Remote HTTP image loading is disabled by default because profile and event URLs
are network-authored. Explicitly opt into the small `AsyncImage` policy with
`.nmpImageLoader(.system)`, or replace image policy for a subtree:

```swift
view.nmpImageLoader(
    NMPImageLoader { url in
        MyAuthenticatedImage(url: url)
    }
)
```

The same leaves remain available below composed cards:

```swift
NMPNIP05(profile.nip05 ?? "")
NMPAvatarGroup(people: people, maximumVisible: 4)
NMPArticleImage(article: article)
NMPArticleTitle(article: article)
NMPArticleSummary(article: article)
NMPArticleByline(article: article, authorProfile: profile)
NMPArticleReadingTime(article: article)
```

## Following resource and button

Following is intentionally different from the controlled reaction primitives.
NIP-02 kind:3 is a whole-list replacement, so an app-supplied `isFollowing`
Boolean plus callback would export destructive protocol logic into every app.

Construct one bindable NMP resource and pass it wherever the relationship is
rendered:

```swift
@StateObject private var following: NMPFollowing

init(engine: NMPEngine, pubkey: String) throws {
    _following = StateObject(
        wrappedValue: try NMPFollowing(engine: engine, target: pubkey)
    )
}

var body: some View {
    NMPFollowButton(following: following, variant: .compact)

    NMPUserCard(
        pubkey: following.target,
        profile: profile,
        following: following
    )
}
```

`NMPFollowing` observes the active account's canonical NIP-02 relationship and
copies NMP's closed availability/action state onto the main actor. The button
owns only styling, accessibility, and its reduced-motion-aware confirmation
animation. Its tap invokes `following.toggle()`; all acquisition, kind:3
preservation, exact-base conflict detection, signing, author-outbox routing,
and receipt handling remain in NMP.

The production state vocabulary distinguishes signed out, acquiring, ready,
cached-only, and source-unavailable from following/not-following/unknown. The
button is enabled only for an established ready relationship and never
optimistically flips a local Boolean.

Apps with their own component can skip `NMPFollowing` and consume the simple
engine action directly:

```swift
let action = engine.follow(pubkey) // or engine.unfollow(pubkey)
for await status in action.status {
    // acquiring, noChange, receipt facts, or typed failure
}
```

Malformed target, signed-out state, acquisition failure, conflict, and relay
outcomes all arrive as action state; `follow` itself does not throw. See
[Editing replaceable state safely](15-editing-replaceable.md) for the
source-scoped base contract.

## Ready-made component catalog

The first SwiftUI family includes:

- `NMPAvatar`, `NMPName`, `NMPNIP05`, `NMPProfileIdentity`, and
  `NMPAvatarGroup`;
- `NMPProfileMention` in text, avatar-name, and pill variants, with optional
  long-press profile preview;
- `NMPEventChrome` in compact, standard, and editorial compositions, accepting
  arbitrary content and footer slots;
- `NMPArticlePortraitCard`, a large editorial feature composition;
- `NMPArticleMediumCard`, a materially different horizontal list/embed
  composition;
- `NMPArticleImage`, `NMPArticleTitle`, `NMPArticleSummary`,
  `NMPArticleByline`, and `NMPArticleReadingTime` as reusable card leaves;
- `NMPUserCard` in featured, landscape, and compact compositions;
- `NMPFollowButton` in compact, prominent, and icon variants, backed by the
  NMP-owned `NMPFollowing` relationship/action resource;
- `NMPReactionButton` in heart, spark, and minimal variants;
- `NMPAvatarReactionButton` and `NMPEmojiReactionBar`.

Article reading time is presentation-derived through `NMPReadingTime`; it is
not inserted into a NIP-23 protocol value. Reaction views remain controlled
components: the host supplies selected/count/people state and actions until the
typed NIP-25 work tracked in issue #155 lands. The follow button does not use
that controlled-state pattern because NMP ships the NIP-02 resource and
semantic action itself.

These components do not transfer their unrelated protocol ownership into
`nmp-content`. For example, `NMPFollowing` observes the active account's
canonical kind:3 relationship and invokes NMP's guarded follow/unfollow action;
it is not part of reference parsing.

## Theme and actions

Renderer sets are immutable ordinary values. Different screens can use
different local values. Themes, navigation, and product actions never affect
demand identity.

```swift
NostrContent(
    document: document,
    observationFactory: observations,
    renderers: renderers,
    actions: NostrContentActions(
        openProfile: router.profile,
        openEvent: router.event,
        openURL: router.url,
        openHashtag: router.hashtag
    )
)
.nmpUITheme(myTheme)
```

## Previews and tests

There is no scripted content-session API. Parse a fixture document and choose
one of three honest inputs:

1. `.literalReferences` for a no-acquisition preview;
2. a custom renderer with explicit fixture values;
3. an injected `NMPReferenceObservationFactory` whose independently owned
   handles deliver fixture `RowBatch` values and cancel deterministically.

The factory seam tests component lifecycle; it does not pretend to be a second
event store or shared session.

## Run the real Gallery

The source of truth is `apps/UIGallery/project.yml`:

```sh
scripts/build-swift-xcframework.sh --sim-only
cd apps/UIGallery
xcodegen generate
```

Build and run `NMPUIGallery`. The app imports the exact package components and
uses `NMPReferenceObservationFactory.live(engine:)`, configures only
`purplepag.es` and `relay.primal.net`, and hardcodes real profile/article/note
entities. Its article and note seeds carry no relay URL; the Live proof shows
ordinary kind:10002 discovery and outbox routing.

The conformance surfaces include literal references with zero acquisition,
standard profile/event components, misleading-kind dispatch, cycle/depth
stops, unknown-kind fallback, missing media, Dynamic Type, RTL, reduced motion,
dark appearance, and long Markdown. The Stress tab mounts production content
rows and reports engine wire-subscription evidence rather than a UI-owned claim
counter or hydration budget. Those deterministic surfaces remain separate from
the live relay proof so network availability cannot disguise a rendering
regression.

## Migration checklist

- Replace `NostrContent(session:...)` with `NostrContent(document:...)` (or the
  `content:` convenience) and pass an observation factory only where selected
  components may resolve references.
- Replace `NMPResolvedProfile(session:pubkey:)` with a component that owns the
  kind:0 observation, or use `NMPStandardProfileMention`.
- Delete `NMPReferenceClaimModifier`, `NostrContentClaim`, session pause/resume,
  scripted sessions, and hydration counters. None has an alias.
- Move custom acquisition policy into `.profileReference` or `.eventLoader`.
- Keep `.event(kind:purpose:)` and `.fallbackEvent` presentation-only; dispatch
  on the acquired row's actual kind.
- Pass `NostrContentRenderContext` through nested loaders; do not introduce a
  mutable budget/coordinator replacement.

See [Mixed Nostr content and reference plans](34-content.md) for the complete
cross-platform ownership and clean-break migration table.
