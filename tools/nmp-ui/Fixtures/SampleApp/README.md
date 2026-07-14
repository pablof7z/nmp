# NMP UI source-install fixture

This target compiles `article-medium-card` and its app-owned `action-surface`
dependency while linking the real `NMPContent` and `NMPUI` products from this
repository. It contains no engine, parser, content-session, renderer registry,
theme provider, or image loader implementation.

The checked-in `Components` and `.nmp-ui-lock.json` are the exact result of:

```sh
swift run --package-path ../.. nmp-ui --root . add article-medium-card
```

After the NMP Swift package's generated native artifacts are present (see
`Packages/NMP/README.md`), build this fixture with:

```sh
swift build --package-path tools/nmp-ui/Fixtures/SampleApp
```
