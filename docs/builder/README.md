# NMP Builder Guide

> **Provisional v2 North Star.** This guide writes forward to the developer
> experience NMP is trying to earn. The behavior is the design contract; names
> and record layouts are illustrative until v2. For the honest shipping surface,
> use [Current implementation status](03-status-map.md).

NMP is an embeddable Nostr sync-and-routing engine. An app gives it two kinds
of work:

1. a **live query** describing data to keep locally available; and
2. a **write intent** describing a publication obligation.

NMP owns the canonical store, dependency resolution, relay planning, sync,
signing orchestration, durable outbox, receipts, and diagnostics. Your app keeps
its architecture, state, screens, account UX, ordering, formatting, and product
policy.

There is no NMP application container, feed framework, subscription manager,
or app lifecycle to adopt.

## Start with the product shape

Read these in order:

1. [The mental model](02-mental-model.md)
2. [A ten-minute embedding](04-ten-minute-timeline.md)
3. [The live-query grammar](09-binding-grammar.md)
4. [Query snapshots and evidence](10-consuming-results.md)
5. [Writing and receipts](14-writing.md)
6. [Protocol modules and composition](32-extending.md)
7. [Permanent diagnostics](22-diagnostics.md)

That path is enough to evaluate the thesis: does NMP feel like a normal data
library, or like an application framework in disguise?

## Core guide

### Understand the boundary

- [Why NMP exists](01-why-nmp.md)
- [The mental model](02-mental-model.md)
- [The two nouns and ownership](05-two-nouns.md)
- [What NMP does not own](29-not-do.md)

### Embed it

- [A ten-minute embedding](04-ten-minute-timeline.md)
- [Native platform shapes](06-first-app.md)
- [Brownfield adoption](07-brownfield.md)
- [Packaging](08-packaging.md)

### Read

- [Live queries and the binding grammar](09-binding-grammar.md)
- [Query snapshots and evidence](10-consuming-results.md)
- [Evidence without global completeness](11-coverage.md)
- [Delivery-side app transforms](13-delivery-transforms.md)

### Write

- [Drafts, acceptance, pending rows, and receipts](14-writing.md)
- [Editing replaceable state safely](15-editing-replaceable.md)
- [Current pubkey and signer selection](16-identity.md)

### Route and operate

- [Source authority and protocol routing context](17-relays.md)
- [Tracing compiled demand](18-tracing-demand.md)
- [Offline acquisition and reconnect](19-offline-sync.md)
- [Signer, AUTH, and crypto capabilities](20-capabilities.md)
- [Provenance and private routes](21-provenance.md)
- [Permanent diagnostics](22-diagnostics.md)
- [Threading and bounded delivery](23-threading-lifecycle.md)
- [Cost, coalescing, and limits](24-performance.md)
- [Testing an embedding app](25-testing.md)
- [Troubleshooting from evidence](26-troubleshooting.md)

### Extend without specializing core

- [Reusable declarations and protocol operations](27-recipes-and-choosing.md)
- [Guarantees and bug classes](28-patterns.md)
- [Platform projections](30-platform-guides.md)
- [Kind-diverse examples](31-gallery.md)
- [Protocol modules and immutable composition](32-extending.md)
- [Mixed Nostr content and live references](34-content.md)
- [Governed provisional API changes](33-versioning.md)
- [Glossary](glossary.md)
- [Open design: bounded/windowed observation](12-collection-mode.md)

## Current truth

The guide above intentionally describes one coherent target rather than mixing
shipping and planned syntax line by line. These pages keep implementation truth
separate and explicit:

- [Current implementation status](03-status-map.md)
- [Known gaps](../known-gaps.md)
- [The governed v2 work queue](https://github.com/pablof7z/nmp/issues/43)

An example in this guide is not proof that its spelling ships today. A behavior
marked as a North Star invariant is not permission to quietly weaken it to fit a
current API.

## The content-neutrality rule

Core examples use caller-selected kinds and literal or generic derived demand.
Protocol-aware examples name the opt-in module that owns their meaning. NIP-02
may provide a reusable follows binding; NIP-29 may provide group operations;
NIP-68 may build a photo draft. None of those becomes a core content default.

The engine must remain useful when an app enables none of them.

## The evidence rule

A query snapshot reports cached rows and scoped facts about the sources NMP
planned. It never claims to know the complete global Nostr result. Exact relay
filters, AUTH state, EOSE, watermarks, counts, caps, and errors remain available
through diagnostics.

## The write rule

Every write returns observable receipt facts. Durable acceptance means the
obligation and canonical pending row were committed together. Explicitly
non-durable writes state their weaker retention promise; they do not become
silent fire-and-forget calls. `Accepted`, `sent`, `acked`, and product-level
success are different facts.

Authoring guidance for this manual lives in [the design guidelines](000-design-guidelines-and-toc.md)
and [writing brief](001-writing-brief.md).
