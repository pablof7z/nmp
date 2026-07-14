# nmp-ui

`nmp-ui` installs editable SwiftUI compositions into an application's source
tree while leaving protocol parsing, live content sessions, and correctness-
sensitive UI primitives in the linked `NMPContent` and `NMPUI` products.

From the NMP repository:

```sh
swift run --package-path tools/nmp-ui nmp-ui list
swift run --package-path tools/nmp-ui nmp-ui view article-medium-card
swift run --package-path tools/nmp-ui nmp-ui --root /path/to/App add article-medium-card
```

`add article-medium-card` writes exactly two app-owned files:

- `Components/NMPUI/ActionSurface.swift`
- `Components/NMPUI/ArticleMediumCard.swift`

It also writes `.nmp-ui-lock.json`, which records the installed dependency
graph, component versions, hashes, and complete upstream bases. Add the two
Swift files to the consuming target when the host build system does not include
that directory automatically. The source continues to import `NMPContent` and
`NMPUI`; the command never copies the NMP engine, content parser, content
session, or linked primitives.

## Wiring boundaries

The installed card accepts the typed `NostrArticle` value, optional profile
metadata, and one host navigation action. That action is intentionally local
to the card:

```swift
NMPSourceArticleMediumCard(article: article) {
    route.openArticle(article.eventID)
}
```

Rich-body renderer and action policy remains explicit at the `NostrContent`
render root. It is not a global registry and is not hidden in the installed
card:

```swift
NostrContent(
    session: articleSession,
    purpose: .detail,
    renderers: appRenderers,
    actions: NostrContentActions(
        openProfile: route.openProfile,
        openEvent: route.openEvent,
        openURL: route.openURL,
        openHashtag: route.openHashtag
    )
)
```

Theme and remote-media policy use the linked `NMPUI` environment boundaries.
Apply them to any subtree that contains installed components:

```swift
NMPSourceArticleMediumCard(article: article, action: openArticle)
    .nmpUITheme(appTheme)
    .nmpImageLoader(.system)
```

The default image loader is disabled. Choosing `.system` or a custom
`NMPImageLoader` is an app policy decision; source installation never enables
remote fetches. Likewise, `NMPUITheme` is a replaceable value scoped to the
chosen subtree, not an app-root provider requirement.

## Updating owned source

```sh
nmp-ui --root /path/to/App diff article-medium-card
nmp-ui --root /path/to/App update article-medium-card
```

Clean files fast-forward. Local and upstream edits are merged against the exact
locked base. A conflict exits non-zero, leaves ordinary conflict markers plus
`.nmp-ui-conflicts.json`, and leaves `.nmp-ui-lock.json` at the last honestly
installed version. Resolve the evidence before attempting another update.

`Fixtures/SampleApp` is the build fixture for this contract. Its package target
links the real `NMPContent` and `NMPUI` products and compiles the two installed
files as ordinary app source.
