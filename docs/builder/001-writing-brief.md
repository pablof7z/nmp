# Builder guide writing brief

## Audience

A Swift, Kotlin, or Rust developer who knows ordinary application architecture
and enough Nostr to recognize filters and events. They want correct data and
routing without adopting an NMP-shaped app.

The guide should let that developer imagine using the settled product before
all public spellings exist. It is a design instrument as well as documentation:
awkward examples are evidence that the API frame needs work.

## Voice

- Direct and concrete.
- Explain ownership through observable behavior.
- Prefer one representative code block to prose about possible APIs.
- Use Nostr terms where they matter; define them on first use.
- Do not sell. State the invariant and what the app no longer has to own.
- Do not narrate repository history in the main learning path.

## North Star versus current truth

The guide landing page carries the provisional-target banner. Concept chapters
then use one coherent imagined API. The status appendix owns the shipping map.

Use a local warning only when one of these is true:

1. copying the target example into the current SDK would be dangerous;
2. current behavior contradicts the target in a way builders must know; or
3. a feature is not committed for v2 at all.

Do not label every section `CURRENT`, `PARTIAL`, or `TARGET`. That makes the
product impossible to read as a product.

## Canonical example vocabulary

### Core query

Use a caller-owned kind and literal authors for the first query. Introduce
`Reactive(CurrentPubkey)`, `Derived`, and `SetOp` as independent grammar
features. A NIP-02 follows fragment belongs in the NIP-02/module section, never
as the core hello-world query.

### Query snapshot

Show `rows`, compact cache evidence, per-source acquisition facts, and
shortfall. Never reduce those facts to one health/completeness enum.

### Core write

Use an immutable unsigned draft for a caller-owned kind. Show:

- default signer from current pubkey;
- explicit per-write identity override;
- `Accepted` before signing when the durable obligation is committed;
- the same pending row becoming signed without changing id;
- per-relay receipt facts; and
- explicitly non-durable status without persistence claims.

### Protocol composition

Use the agreed cross-owner example:

```text
asset   = Blossom.upload(file)
photo   = Nip68.buildPhoto(asset)
receipt = nip29.group(groupId, hostRelay).publish(photo)
```

Spell out that Blossom owns bytes, NIP-68 owns the photo draft, NIP-29 owns
only group context (`h` plus host authority), and core signs once.

## Reader journey

1. **Mental model:** two nouns and the ownership boundary.
2. **Quickstart:** construct, observe a literal query, inspect evidence,
   publish a draft, watch a receipt.
3. **Read:** binding graph, snapshot facts, bounded/windowed observation.
4. **Write:** durable acceptance, pending row, signer selection, receipts.
5. **Protocols:** reusable closed declarations and semantic module operations.
6. **Operate:** diagnostics, source plans, limits, offline/retry evidence.
7. **Integrate:** native projection and brownfield ownership.
8. **Current status:** honest map of what compiles and what remains open.

## Chapter ownership

| Chapter family | Owns |
|---|---|
| `01`-`05` | thesis, mental model, status appendix, quickstart, ownership |
| `06`-`08`, `30` | platform projection, embedding, packaging |
| `09`-`13` | live-query grammar, snapshots, evidence, bounded delivery |
| `14`-`16` | writes, replaceable edits, identity/signers |
| `17`-`21` | source authority, routing, sync, capabilities, provenance |
| `22`-`26` | diagnostics, pressure, tests, troubleshooting |
| `27`-`29`, `31`-`33` | modules, guarantees, examples, extension, governance |

## Review checklist

- Does the chapter preserve the two nouns?
- Is core still useful with no protocol module enabled?
- Did a social/content example accidentally become a core default?
- Can every engine decision be printed and explained?
- Are query facts scoped to known sources instead of global truth?
- Does every write have observable status without equating acceptance and ack?
- Is current pubkey separate from optional signer override?
- Does protocol composition preserve exact ownership and immutable drafts?
- Is app architecture still plainly app-owned?
- Are target spelling and current availability separated honestly?
