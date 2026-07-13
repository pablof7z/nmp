# SwiftUI content and components

`NMPUI` is the optional native UI product above `NMPContent`. It opens no
socket, owns no event cache, and selects no canonical event. A supplied
`NostrContentSession` delivers the latest shared document and resolved
resources; the views render that state synchronously.

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
let content = NMPContentClient(engine: engine)
let session = content.session(
    content: "hello nostr:npub1... read nostr:naddr1..."
)
```

Render it with useful defaults:

```swift
NostrContent(session: session, renderers: .standard)
```

There is deliberately no `NostrInlineContent`, `NostrBlockContent`, public
`ListBlock`, or public `CodeBlock`. `NostrContent` owns the semantic document
walk. Internally it lays text and arbitrary native views into flow and applies
authored Markdown contexts. Apps choose renderers, not parser plumbing.

## Identity primitives

`NMPAvatar` and `NMPName` are the shared base of mentions, bylines, event
chrome, and user cards:

```swift
NMPAvatar(pubkey: pubkey, profile: profile, size: 44)
NMPName(pubkey: pubkey, profile: profile)
```

Before kind:0 resolves, Avatar uses a deterministic pubkey-derived color and
initials while Name uses a stable abbreviated pubkey. A resolved image never
changes layout. Remote HTTP image loading is disabled by default because profile
and event URLs are network-authored. Explicitly opt into the small `AsyncImage`
policy with `.nmpImageLoader(.system)`, or replace image policy for any subtree:

```swift
view.nmpImageLoader(
    NMPImageLoader { url in
        MyAuthenticatedImage(url: url)
    }
)
```

`NMPResolvedProfile(session:pubkey:)` is the connected convenience. It claims
kind:0 through the existing content session and supplies the optional profile
to arbitrary child views; it does not independently query.

The same leaves are available below the composed cards:

```swift
NMPNIP05(profile.nip05 ?? "")
NMPAvatarGroup(people: people, maximumVisible: 4)
NMPArticleImage(article: article)
NMPArticleTitle(article: article)
NMPArticleSummary(article: article)
NMPArticleByline(article: article, authorProfile: profile)
NMPArticleReadingTime(article: article)
```

## Replace one renderer

Renderer sets are immutable ordinary values. The last explicit builder call on
your local value replaces that key; another screen can use a different value.

```swift
let previewRenderers = NostrContentRenderers.standard
    .profileMention { input in
        NMPProfileMention(
            pubkey: input.pubkey,
            profile: input.profile,
            variant: .text
        )
    }

NostrContent(
    session: previewSession,
    purpose: .preview,
    renderers: previewRenderers,
    maximumBlocks: 1,
    maximumLinesPerBlock: 2
)
```

This is the channel-preview path: a NIP-27 mention becomes the current name as
kind:0 arrives instead of remaining a bech32 string. It uses the same parser,
claim, query, cache, routing, and replacement path as full content.

## Add an app-only kind

No Rust enum, FFI union, application provider, or global registry changes:

```swift
let renderers = NostrContentRenderers.standard
    .event(kind: 12_938, purpose: .embedded, layout: .block) { input in
        MyPrivateRecordCard(event: input.event)
    }
```

An event renderer receives the validated current row, authored placement,
render purpose, structural recursion context, parent content session, actions,
and the renderer set for nested content. It does not fetch.

## Script previews and deterministic states

Previews and component tests do not need an engine, fake socket, or app-root
provider. A scripted session has the same synchronous rendering contract, but
its claims are inert and it cannot open a query:

```swift
let preview = NostrContentSession.scripted(
    document: document,
    resources: [
        target: .resolved(
            resource: .event(fixtureRow),
            evidence: NostrContentEvidence()
        )
    ]
)

NostrContent(session: preview, renderers: .standard)
```

`Row` has a public raw-value initializer for fixtures and import adapters. It
does not validate or insert anything into NMP. If an application owns a Djot,
AsciiDoc, or custom-kind parser, it constructs `NostrContentDocument` directly
and passes that document to either a live or scripted session. NMP reference
targets inside the document keep the same rendering and acquisition contract.

## Ready-made components

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
- `NMPReactionButton` in heart, spark, and minimal variants;
- `NMPAvatarReactionButton` and `NMPEmojiReactionBar`.

Article reading time is presentation-derived through `NMPReadingTime`; it is
not inserted into the NIP-23 protocol value. Reaction views are controlled
components: the host supplies selected/count/people state and actions. Typed
NIP-25 live resources and write intents are tracked separately in issue #155.

## Theme and actions

Override `NMPUITheme` at any subtree. Supply navigation and product policy as
`NostrContentActions`; components never install routes or own navigation.

```swift
NostrContent(
    session: session,
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

## Run the real Gallery

The source of truth is `apps/UIGallery/project.yml`:

```sh
scripts/build-swift-xcframework.sh --sim-only
cd apps/UIGallery
xcodegen generate
```

Build and run `NMPUIGallery`. The app imports the exact package components,
configures only `purplepag.es` and `relay.primal.net`, and hardcodes real
profile/article/note entities. Its article and note seeds have no relay URL;
the Live proof screen shows the additional author relays NMP discovers through
ordinary kind:10002 outbox routing.

The States tab is the deterministic conformance surface for loading, shortfall,
cycle, unknown-kind, missing media, Dynamic Type, RTL, reduced motion, dark
appearance, and long Markdown. The Stress tab mounts 72 production content
rows with two live references each and exposes the current visible-claim count
while rapidly scrolling. Those tabs are intentionally separate from the live
relay proof so a missing network never disguises a rendering regression.
