# PROCESS.md — NMP Requirements Archaeology (adapted for this environment)

## What this document is

A modified version of the original NMP Requirements Archaeology process, adapted to run in *this* environment rather than a constrained single-agent sandbox.

The original process was written assuming one agent, no access to the human except written rulings, and only the repository + GitHub as evidence. Here we have three capabilities the original lacked, and the process is rewritten to exploit them:

1. **Local conversation transcripts** of the sessions that actually produced NMP — raw user messages to the agents that built it — in `~/.claude/projects/-Users-pablofernandez-Work-nmp*` (per-project `.jsonl` session transcripts; ~6 NMP-related project dirs including worktrees) and `~/.codex/sessions/**` (Codex rollouts; ~5964 session files across all projects, filter to NMP cwd), plus distilled `~/.codex/memories/rollout_summaries/*.md` (256 total, 29 NMP-tagged) and `~/.codex/memories/MEMORY.md` / `raw_memories.md`. These are the richest possible source of *human intent*: what was actually asked for, corrected, rejected, and why — captured in real time, not reconstructed after the fact from issues.
2. **Parallel subagents** — the lead can fan out bounded exploration tasks and synthesize their findings.
3. **Direct access to the human** — the project owner is available for quick review passes, so we can run a fast first cut, present it, and let the owner point at what's missing rather than committing to 6–10 long autonomous rounds.

The mission, the sole deliverable (`SPEC.md`), the core principles, the evidence discipline, the qualification test, falsification, destructive simplification, and the completion gate are unchanged. What changes is *how the research is executed*: a richer evidence base, parallelized exploration, and tighter human loops.

---

## Mission (unchanged)

Reconstruct the requirements worth carrying into a clean rewrite of NMP.

The sole final deliverable is `SPEC.md`: a technically rigorous, implementation-neutral description of what the new system must do.

It must capture:

* application-facing capabilities;
* observable behavioral guarantees;
* offline, failure, race, restart, and recovery semantics;
* internal invariants only where strictly necessary to guarantee externally meaningful behavior;
* clear ownership boundaries between NMP, applications, signers, protocol-specific capabilities, UI layers, and other systems;
* explicit non-requirements where historical complexity is likely to be accidentally recreated.

Existing crates, modules, APIs, types, state machines, names, queues, registries, abstractions, and implementation strategies are **evidence to investigate, not requirements to preserve.**

The goal is not to understand the existing architecture well enough to recreate it.
The goal is to recover the smallest coherent behavioral contract that preserves what NMP was actually intended to provide.

Do not begin merely because this document exists. Begin only when explicitly instructed.

---

# 1. Core principles

## 1.1 Start from behavior, not architecture (unchanged)

**BDD/docs → executable behavior → public surfaces → decision history → implementation → pruning**

Do not begin by understanding crates or internal subsystems. Start with what applications are supposed to experience. For every existing mechanism eventually encountered ask: what useful behavior is this trying to guarantee; is it still wanted; does NMP need to own it; is the mechanism necessary or one implementation; what breaks for a real application if it disappears.

## 1.2 BDD scenarios and docs are the initial map, not the truth (unchanged)

They seed candidates; they do not establish the final specification. Each is corroborated, historically traced, and attacked.

## 1.3 Treat the repository as contaminated evidence (unchanged)

The repo mixes deliberate requirements, settled invariants, experiments, temporary architecture, agent-generated generality, speculative extensibility, baggage, superseded requirements, and dead machinery. Momentum does not establish intent.

## 1.4 Human intent outranks repository momentum (unchanged, but now operationalized)

Explicit human rulings are the strongest evidence. **In this environment, human intent is recoverable at scale from local transcripts** (see §2 Class A0), so it is front-loaded rather than discovered late. When repository evidence cannot establish intended scope, ask the human directly (see §13) rather than silently promoting complexity into `SPEC.md`.

## 1.5 Recover behavior, not existing vocabulary (unchanged)

Write for a Nostr developer who has never seen NMP. Prefer NIP terminology → ordinary software terminology → broadly established Nostr-library terminology → a new NMP-specific term only when genuinely distinct. Current NMP vocabulary (`Demand`, `Binding`, `Atom`, `ContextualAtom`, `Coverage`, …) is suspect by default.

## 1.6 Let `SPEC.md` discover its own structure (unchanged)

Research lenses must not dictate final structure. Begin with Purpose / Requirements / Non-requirements / Unresolved. Reorder freely around surviving product concepts. Do not organize around crates, modules, type families, research phases, or issue groupings.

## 1.7 Preserve implementation freedom (unchanged)

Requirements describe the result that must hold, not the mechanism currently used. If `SPEC.md` effectively forces reconstruction of today's architecture, the archaeology has failed.

---

# 2. Evidence hierarchy (modified)

Classify evidence by strength.

## A0 — Raw human intent from local session transcripts *(new, strongest)*

Direct user/owner messages to the agents that built NMP. Source corpus:

* `~/.claude/projects/-Users-pablofernandez-Work-nmp*/**/*.jsonl` — Claude Code session transcripts for the NMP repo and its worktrees (6 project dirs).
* `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` filtered to sessions whose cwd is the NMP repo (the Codex `session_index.jsonl` carries thread names; rollouts are JSONL).
* distilled summaries: `~/.codex/memories/rollout_summaries/*nmp*.md` (29 NMP-tagged) and `~/.codex/memories/MEMORY.md`, `raw_memories.md`, `memory_summary.md`.

**Canonical extraction tool — `pc` (proactive-context):** do not hand-parse raw JSONL. Use the `pc` binary at `/Users/pablofernandez/src/proactive-context/target/release/pc`:

* `pc recall dump --cwd /Users/pablofernandez/Work/nmp --include-subdirs --format markdown [-o FILE] [--provider both|claude|codex] [--no-archived-codex] [--raw-text]` — emits a cleaned, deduplicated, project-scoped transcript dump. Verified output for NMP: 113 sessions / 8646 messages, grouped by worktree branch name, each message tagged `#### user` or `#### assistant` with timestamp and a pointer to the original transcript line. **The `#### user` blocks are Class A0; `#### assistant` blocks are Class E unless independently corroborated.** This tag boundary is the A0/E filter — use it.
* `pc recall index` — builds an index of **human-only utterances** extracted from Claude Code + Codex transcripts (the A0/E separation done for you).
* `pc recall ask "<question>" [--brief] [--chunk] [--model SPEC]` — cited semantic answers over the whole human-only corpus; `--brief` for agent consumption mid-task, `--chunk` map-reduce when corpus > model context. Use this for targeted intent queries during research rounds instead of re-reading dumps.
* `pc archeologist --project <path> [--since DATE] [--dry-run] [--output-dir DIR]` — heavier: replays transcripts through a capture pipeline to populate a per-project wiki. Use only if a distilled wiki is wanted; `recall dump` is the lighter default path.

**Why strongest:** these capture intent, corrections, rejections, and rationale *as they happened*, including the moments complexity entered the system — exactly what the original process had to reconstruct indirectly from issues. Owner messages are Class A0; agent reasoning *within* a transcript is not (it is Class E unless independently corroborated). Treat the owner account's messages as authoritative human voice; treat any other human's messages as context only.

**Caveat:** transcripts are also contaminated — they contain agent proposals the user may have merely failed to reject, experiments, and stale positions later reversed. Apply §1.4 (later explicit corrections supersede earlier). Mine for *what the owner said*, not for what the agent concluded.

## A — Explicit human decision (as before, now corroborated by A0)

Owner explicitly requested / corrected / accepted / narrowed / rejected / clarified behavior. GitHub issue/PR comments by the owner are included here. Later explicit decisions supersede earlier only when they genuinely address the same question; recency alone is insufficient.

## B — Executable behavioral contract (unchanged)

BDD/acceptance scenarios, adversarial falsifiers, integration tests, restart/crash tests, conformance tests, cross-SDK behavioral tests. Strong, but still determine who introduced it, why, whether its premise survived, whether later guidance rejected it.

## C — Repeated invariant with a recoverable reason (unchanged)

Implementation + tests repeatedly enforce behavior whose reason is explainable via Nostr correctness, distributed-systems correctness, offline behavior, persistence/recovery safety, or a concrete application requirement.

## D — Public API, examples, or documentation (unchanged)

Useful evidence of intended behavior; not automatically authoritative. A public type/function is not itself a requirement; source compatibility is not automatically a requirement.

## E — Implementation-only machinery (unchanged)

An abstraction exists without independent evidence that applications rely on its semantics. Default disposition: **do not carry over.** Agent reasoning inside transcripts defaults here too.

## Conflict precedence (unchanged in substance)

1. explicit later human decisions beat earlier ones they directly supersede;
2. human corrections beat agent-authored proposals;
3. explicit scope reductions invalidate machinery that existed only for the removed capability;
4. contemporaneous executable behavior beats unsupported prose unless later rejected;
5. documented application behavior generally outweighs implementation detail;
6. implementation never wins merely because it exists;
7. genuine unresolved product conflicts require a human ruling (ask the user — see §13).

Never average incompatible historical positions into a vague compromise requirement. Reconstruct what changed and why.

---

# 3. How this environment changes execution (the delta)

Three capabilities reshape the workflow. The original phases are preserved but reordered and parallelized around them.

## 3.1 Transcript priming (uses A0)

The single biggest upgrade: human intent is recovered **before** reading implementation, and used as the filter for everything afterwards.

Before descending into internals, build a **Human Intent Timeline** by mining transcripts for owner messages:

* what NMP was originally supposed to be;
* what concrete problems the owner said it should solve;
* every correction ("no, not like that — …");
* every rejection or scope reduction;
* every "this is over-engineered / drop it";
* every ownership decision ("that's the app's job" / "NMP should own that");
* every architecture premise the owner accepted or rejected and the reason given.

This timeline is research scaffolding (not part of final `SPEC.md`). It seeds candidate requirements *and* seeds the explicit-non-requirements list *and* seeds the mechanism-tombstone list — all three at once, from primary source.

The `pc recall dump` output is already cleaned and project-scoped (verified: 113 sessions / 8646 messages for NMP), so the dump *is* the corpus — no raw JSONL in the lead's context. Mining is delegated to parallel subagents (see §3.2) that read partitions of the dump file and return **distilled findings with quotes + the `#### user`/line pointers pc emits**, never raw transcript dumps into the lead's context. For targeted questions during later rounds, prefer `pc recall ask --brief` over re-scanning the dump.

## 3.2 Parallel subagents (uses the Agent tool)

Phases are restructured to fan out bounded exploration and synthesize. Rules:

* **Behavior-first framing.** Every subagent prompt frames its task as a *behavioral question* (or a census/enumeration task), never as "understand crate X." This preserves §1.1 even when work is parallel.
* **Subagents return evidence and findings, not requirements.** Dispositions (KEEP / DROP / NARROWER / APP-OWNED / OPTIONAL / UNRESOLVED) are assigned by the lead after synthesis. This preserves the no-averaging rule (§2) — a subagent must not silently settle a contested requirement.
* **Context hygiene.** Large corpora (transcripts, test suites, persisted state) are mined by subagents that return compact distilled findings; the lead never ingests raw dumps. A subagent that reads a 17MB session returns a few hundred words of quoted intent, not the file.
* **Independence.** Bounded questions that don't depend on each other are dispatched in a single message with `run_in_background: true` so they run concurrently. The lead waits for completion notifications rather than polling.
* **Verifiability.** The lead spot-checks subagent findings against the cited source before promoting them (trust but verify).
* **No speculative fan-out.** Parallelize where the corpus genuinely partitions; don't manufacture parallel work that overlaps and duplicates.

## 3.3 Direct human access / quick review passes (uses the user)

Replace "defer every human question to the end" with a tight loop:

* **Quick first pass.** Run a fast census + transcript priming + BDD/docs seed, produce a **v0 `SPEC.md`** plus an explicit *"what I think I'm missing / what looks suspicious / open product questions"* list, and hand it to the owner for review. The owner points at gaps and redirects far more cheaply than autonomous rounds would discover them.
* **Live checkpoints.** The mandatory per-round checkpoint (§17) can be a *live review* with the owner rather than a written-only artifact — present the current `SPEC.md`, the delta, the open questions, and let the owner steer. `SPEC.md` stays self-contained for resumability regardless.
* **Ask immediately when needed.** A genuine product decision that archaeology cannot resolve is asked now, not queued (§13). But never ask a question further archaeology could answer.
* **Owner as coverage oracle.** Because the owner built the system, the owner can quickly confirm/deny whether a behavioral domain has been missed — use review passes to check coverage against the owner's mental map, not just against corpus counters.

This collapses the original 6–10 long autonomous rounds into: **priming pass → owner review → a small number of targeted rounds informed by owner direction → falsification + destructive pruning → closure**, with owner checkpoints between.

---

# 4. Phase 0 — Baseline, transcript priming, and corpus census (modified)

Before substantive interpretation, establish a stable baseline **and** mine human intent.

## 4.1 Freeze the baseline

Record: repository path + branch, exact commit SHA, research start date, GitHub issue/PR/comment cutoff, the owner account(s) representing authoritative human voice, and any supplied external artifacts. Do not silently follow a moving branch; post-baseline changes go to a delta queue.

## 4.2 Transcript census (parallel, subagent-driven)

Before reading any implementation, enumerate the transcript corpus and extract the Human Intent Timeline. Dispatch parallel subagents:

* one scans `~/.claude/projects/-Users-pablofernandez-Work-nmp*` and inventories session files (count, sizes, date span, cwd per file);
* one scans `~/.codex/sessions/**` via `session_index.jsonl` + rollout metadata and inventories NMP-cwd rollouts (count, date span);
* one inventories distilled summaries (`rollout_summaries/*nmp*`, `MEMORY.md`, `raw_memories.md`, `memory_summary.md`).

Then dispatch transcript-mining subagents over partitions of the `pc recall dump` file (e.g. by date range, or by the worktree-branch session groupings pc emits) that extract **`#### user` messages only** (the A0/E tag boundary is already in the dump) — each returns: quoted intent, source pointer (the transcript file + line pc prints), provisional classification (request / correction / rejection / scope-reduction / ownership-decision / premise). The lead synthesizes these into the Human Intent Timeline and a first cut of explicit non-requirements + candidate tombstones.

Transcripts are noisy; the lead verifies ambiguous quotes against context (or via `pc recall ask`) before treating them as Class A0.

## 4.3 Repository census (parallel, subagent-driven)

Concurrently, enumerate (objective denominators wherever practical):

* **Behavioral starting material:** BDD/acceptance scenarios, application-facing docs, tutorials, examples, demo apps.
* **Executable contracts:** behavioral/integration/restart/crash/adversarial/falsifier/cross-SDK/conformance tests.
* **Public surfaces:** Rust, Swift, Kotlin, and other SDK/binding application-facing exports; relevant config/feature surfaces.
* **Persisted behavior:** persisted state families, queues/journals, canonical event storage, derived state, persisted publication state, migration/recovery.
* **Historical evidence:** relevant GitHub issues, PRs, owner comments/reviews, design docs, reversals, reverts, substantial deletions, replaced public APIs, major pivots.

Do not deeply read all of these yet — the objective is to know what exists and establish coverage counters. Partition across parallel subagents by evidence type.

## 4.4 Initialize scaffolding

Create temporary research ledgers for: candidate requirements, source provenance, terminology translations, unresolved contradictions, explicit human rulings (seeded from the timeline), mechanism tombstones (seeded from the timeline), and corpus coverage. These are scaffolding, not deliverables; the only final deliverable is `SPEC.md`.

---

# 5. Phase 1 — Behavioral seed from transcripts + BDD/docs (modified)

The first substantive phase. Combine transcript priming with the behavioral corpus, rather than treating them sequentially.

## 5.1 Extract candidate requirements

For every explicit behavior (from BDD/acceptance scenarios, application-facing docs, examples, **and** owner statements in the timeline), identify: the application scenario; what the app supplies; what NMP owns; what the app receives; what changes affect the result; lifecycle; failure; offline; restart; ordering; consistency; cancellation; deliberate exclusions. Translate into ordinary Nostr/software language; do not preserve source terminology merely because the scenario uses it.

## 5.2 Cross-check seed against the timeline immediately

Every seed candidate is tagged against the Human Intent Timeline:

* **Corroborated** by an owner statement → strong seed.
* **Neutral** (timeline silent) → ordinary seed.
* **Contradicted** by a later owner correction/rejection → marked CONTRADICTED or non-requirement immediately, not later.
* **Originated only in agent proposals** (no owner acceptance in transcript) → marked SUSPICIOUS / ARCHITECTURE-LEAKING.

This front-loads the work the original process deferred to Phase 3.

## 5.3 Mark evidence quality

Classify each as SEED-A0 (transcript-corroborated), SEED-BDD, SEED-doc, SEED-example, SUSPICIOUS, CONTRADICTORY, ARCHITECTURE-LEAKING, or UNCLEAR.

## 5.4 Behavioral families

Group candidates by conceptual relationship (querying/observation, query dependencies, historical+live, offline, publication, sync, identity, signing, relay, event semantics, restart/recovery, cancellation, cross-platform). Investigation aids only — they do not predetermine `SPEC.md` sections.

## 5.5 Produce v0 `SPEC.md`

At the end of this phase `SPEC.md` holds a meaningful provisional behavioral spec plus a Research State appendix (§18). Then run the **quick first pass** (§3.3): hand the owner (a) the current `SPEC.md`, (b) the Human Intent Timeline digest, (c) an explicit "what I think I'm missing / suspicious / open" list, and let the owner redirect before deeper rounds.

---

# 6. Phase 2 — Corroborate against executable behavior and public surfaces (modified, parallelized)

Now inspect tests and public APIs using the seed candidates as the index. Dispatch parallel subagents partitioned by behavioral family (not by crate), each framed by a behavioral question. The lead synthesizes and assigns dispositions.

## 6.1 Map behavioral tests

For every relevant executable behavioral test, determine: which candidate requirement it supports; whether it reveals an undocumented edge case; whether it contradicts docs/BDD/timeline; whether it enforces an implementation detail rather than useful behavior; whether its motivating feature still exists. Every behavioral test gets one disposition: mapped to a surviving requirement; mapped to an explicit non-requirement/history note; obsolete; implementation-only; or unresolved with a reason.

## 6.2 Inspect public API semantics

For every application-facing surface, determine whether it represents a genuine semantic capability, a convenience API that may be redesigned, a platform adaptation, an experimental surface, legacy compatibility, accidental exposure, or obsolete functionality. A public API being public does not mean its naming, type structure, or source compatibility must survive. Ask what application capability it actually exposes.

## 6.3 Identify missing semantics

Use tests and APIs to enrich the seed with things prose omitted: cancellation, ordering, multiple observers, duplicate delivery, replaceable/addressable events, partial failures, restart, invalidation, stale state, races, cross-SDK parity, stream termination. These become candidate obligations subject to later historical validation.

---

# 7. Phase 3 — Requirement-driven archaeology (modified, parallelized)

Only after a meaningful behavioral spec exists does research descend deeply into implementation, git history, issues, PRs, reviews, design docs, deleted code, and superseded architecture. The unit of research is a **behavioral question**, never a crate.

Good: "What guarantees should a reactive query preserve when the follow/mute relationships it depends upon change?"
Bad: "How does `ResolverGraph` work?"

## 7.1 Preferred trace direction (unchanged)

BDD/docs → tests → public API → decision history → implementation. The Human Intent Timeline (A0) sits alongside decision history and is checked first when a question is disputed.

## 7.2 Reconstruct the decision history

For evolved or disputed behavior, establish: the original problem; first proposed behavior; machinery introduced; bugs/counterexamples; agent-generated assumptions introduced; human corrections; scope reductions; later simplifications; latest surviving intent; obsolete consequences still present. Use issues, PR discussion, review comments, commit history, rename-aware history, deleted tests, reverts, follow-up issues, owner corrections — **and the transcript timeline, which often captures the "why" the issues omit.**

## 7.3 Extract the smallest surviving contract

For every candidate answer: application value; observable contract; ownership (why NMP rather than app/UI/signer/NIP-helper/other library); edge cases (races, reconnects, duplicates, cancellation, partial failure, restart, stale state, missing signer, account changes); implementation freedom; exact supporting sources.

## 7.4 Assign a disposition

KEEP / KEEP, NARROWER / APP-OWNED / OPTIONAL CAPABILITY / DROP / UNRESOLVED. Requirements depending on a dropped premise are reevaluated. Don't keep dead requirements alive because deleting their machinery looks inconvenient.

## 7.5 Parallelization protocol

Behavioral questions that don't depend on each other are dispatched as parallel subagents in one message. Each subagent returns findings + evidence pointers, framed behavior-first. The lead synthesizes, resolves cross-question conflicts, assigns dispositions, and verifies spot findings. Contested questions that parallel research cannot resolve are escalated to the owner (§13) immediately rather than queued.

---

# 8. Requirement qualification test (unchanged)

A candidate becomes normative only after surviving:

* **Intent** — real application need; explicitly requested; Nostr-correctness; offline/recovery guarantee; motivating scenario later rejected?
* **Observability** — can the app observe the difference; if not, is the internal invariant strictly necessary for something observable; could a different implementation satisfy it?
* **Ownership** — why must NMP own this; could apps/UI/signers/NIP helpers own it; is it generic-library behavior or optional-protocol-only?
* **Precision** — trigger, result, lifecycle, ordering, consistency, failure, cancellation, restart stated where they matter; no slogans ("offline-first", "reactive", "reliable", "durable", "correct") — spell out consequences.
* **Falsifiability** — every normative `MUST` has at least one plausible acceptance test or adversarial scenario capable of proving the implementation wrong.
* **Implementation neutrality** — does the statement accidentally require an existing crate/registry/reducer/state-machine/actor/database/queue/type-hierarchy? If so, rewrite around the semantic obligation.

---

# 9. Phase 4 — Reverse completeness sweep (modified, parallelized)

Behavior-first research can miss behavior absent from the BDD/docs seed. Sweep to discover it. Partition across parallel subagents by surface class.

* **9.1 Public surface sweep** — every application-facing public capability classified; no unexplained exported behavior remains.
* **9.2 Behavioral test sweep** — every relevant behavioral test mapped to a surviving requirement, an explicit non-requirement, obsolete behavior, implementation-only, or unresolved.
* **9.3 Persisted state sweep** — for every significant persisted state family, ask what externally meaningful guarantee would disappear if this persistence didn't exist. Outcomes: real restart/durability requirement; cache/performance only; rebuildable derived state; historical baggage; machinery for a removed feature. Persistence is not automatically a requirement.
* **9.4 Human ruling sweep** — every identified owner ruling (from GitHub *and* the transcript timeline) is represented, superseded, deliberately excluded, or unresolved. Especially important where code hasn't caught up with intended scope.
* **9.5 Deletion and revert sweep** — review important deletions/reverts/replaced architectures for both risks: accidentally losing a valid requirement whose old impl was removed; accidentally carrying machinery whose requirement was rejected.
* **9.6 Cross-platform sweep** — check whether Rust/Swift/Kotlin/other SDKs encode semantic contracts absent from the main spec; investigate stream behavior, cancellation, concurrency, errors, ownership, lifecycle, ordering, parity. Don't preserve accidental FFI shapes as requirements.

After the sweep, run an **owner coverage checkpoint** (§3.3): present the candidate domain coverage to the owner and ask what's missing from their mental map. The owner is a coverage oracle the original process lacked.

---

# 10. Phase 5 — End-to-end falsification (unchanged)

Requirements can look correct individually while inconsistent when composed. Construct complete application journeys and attack the spec with them. Journeys are falsification tools, not automatic requirements. Include: fully offline start; cached results immediately available; later reconnect; remote state changed while offline; reactive query depends on follows/mutes/lists; dependency changes and result must update without app orchestration; cached event becomes newly eligible due to dependency change; several relays deliver the same event; historical sync overlaps a live subscription; process exits while observing live data; process exits after accepting a publication but before relay completion; some relays accept and others fail; signer unavailable; account changes; consumer cancels while related work stays useful to another; overlapping consumers; historical pagination overlaps incoming live events.

For each journey: derive expected behavior only from existing candidates; find ambiguity/contradiction; determine whether a missing requirement exists; trace it through evidence; do not invent semantics merely to make the scenario tidy.

---

# 11. Phase 6 — Destructive simplification (unchanged)

Attack the entire draft for deletion and narrowing. For every major requirement ask: what real application breaks if this disappears; was it ever actually requested; was the motivating scenario fabricated/speculative; did later guidance reject the premise; could the app own it; does it belong only in an optional protocol capability; does it support another questionable requirement; is runtime dynamism really necessary; is generic extension machinery really necessary; is multiple-backend abstraction actually required; is compatibility genuinely required; can a more general invariant replace several detailed requirements; can it be narrowed substantially; is an unrealistic edge case creating disproportionate machinery. A requirement survives only if its value remains clear after this attack.

---

# 12. Terminology decontamination (unchanged)

Maintain a temporary terminology ledger: repository term → plain-language interpretation → current disposition. For every suspicious term: identify actual behavior; translate to plain language; compare with NIP terminology; compare with ordinary Nostr-library terminology; decide whether the concept survives; introduce a special term only if it genuinely improves precision. The ledger is not part of final `SPEC.md`.

---

# 13. Human rulings (modified — ask now, don't queue)

Ask the owner only when further archaeology (including transcript mining) cannot resolve genuine product intent. Don't ask because investigation is inconvenient. Good reasons: evidence supports two materially different intended behaviors; an agent introduced a feature with no clear owner acceptance in transcripts; ownership between NMP and app is unclear; a generic capability might have been intended only for one optional NIP; current code supports a behavior later comments appear to reject.

Each question carries: **Finding** (what exists), **Ambiguity** (why evidence can't resolve scope), **Consequence** (what spec complexity depends on the answer), **Assessment** (researcher's current conclusion when useful), **Options** (a small concrete set, including narrower/app-owned alternatives). Record the ruling as KEEP / KEEP, NARROWER / DROP / APP-OWNED / OPTIONAL CAPABILITY / CONTINUE RESEARCH / DELIBERATELY UNRESOLVED. Apply to all dependent requirements. Unanswered questions must not block unrelated research.

**Environment delta:** because the owner is reachable, rulings are requested as they arise, not batched to the end. A live checkpoint can resolve several at once.

---

# 14. Requirement records (unchanged)

Assign a stable ID per surviving candidate (e.g. `REQ-QUERY-001`). Do not renumber surviving IDs when another is removed; retired IDs remain in provenance. Each material record captures: Title (plain); Status (SEED/CANDIDATE/CORROBORATED/CONTESTED/FINAL/REJECTED/SUPERSEDED/DEFERRED); Normative statement (MUST/SHOULD/MAY, defined once); Application value; Observable semantics; Trigger and dependencies; Lifecycle; Ordering and consistency; Offline/restart semantics; Failure semantics; Explicit exclusions; Acceptance criteria (≥1 falsifiable criterion per MUST); Ownership; Evidence (precise provenance, including transcript pointers for A0); Dependencies/conflicts; Confidence. Detailed provenance may be collected in a traceability section to keep the body readable.

---

# 15. Mechanism tombstones (unchanged; seeded earlier here)

For rejected mechanisms attractive enough that future agents may reinvent them, record: what existed; what premise motivated it; evidence the premise was rejected; the smaller surviving requirement if any; the instruction that the rewrite must not recreate the mechanism by default. Use selectively; don't turn `SPEC.md` into a catalog of every deleted type. **Here, tombstones are seeded from the transcript timeline during Phase 0/1** — owner rejections often name the rejected mechanism directly.

---

# 16. Research-round workflow (modified)

After census + transcript priming + behavioral seed, organize work into bounded behavioral/decision threads. A round answers one bounded question. Good: "What guarantees should a live query preserve when the values it depends upon change?" / "What publication state, if any, must survive a restart?" / "What correctness requirement exists when historical reconciliation overlaps live subscriptions?" Bad: "Investigate `nmp-engine`."

Per round: (1) define the behavioral question; (2) define exclusions; (3) identify relevant existing requirements; (4) enumerate evidence sources; (5) inspect behavioral evidence first (timeline included); (6) trace decision history; (7) inspect implementation only to recover invariants or understand historical consequences; (8) extract/revise candidates; (9) attack each candidate; (10) assign dispositions; (11) normalize terminology; (12) update acceptance criteria; (13) update coverage; (14) update the complete `SPEC.md`; (15) publish the checkpoint (live with the owner where possible); (16) record the exact next recommended batch. Steps 5–7 may be parallelized across subagents per §7.5.

---

# 17. Mandatory checkpoint after every round (modified — may be live)

## 17.1 Currently known requirements
Complete cumulative inventory, independently understandable without prior checkpoints: behavior, implications, offline/restart where relevant, failure/race where relevant, ownership, status, confidence, primary evidence.

## 17.2 New findings in this round
Skimmable delta: added/strengthened/narrowed/merged/dropped requirements, disproved assumptions, obsolete mechanisms, explicit non-requirements, contradictions found/resolved, terminology corrected, human rulings incorporated, meaningful restructuring. No restating unchanged material.

## 17.3 Progress / remaining work / ETA
Progress by evidence coverage, not time. Counters established from the census (e.g. BDD scenarios classified X/Y; docs/examples reviewed X/Y; behavioral tests mapped X/Y; public symbols classified X/Y; persisted families investigated X/Y; owner rulings incorporated X/Y; pivots/reverts investigated X/Y; cross-SDK contract groups reviewed X/Y; transcript owner-messages triaged X/Y; unresolved rulings N). Also: baseline commit; completed domains/questions; high-risk unexamined areas; unresolved dependencies; next recommended round; estimated remaining substantive rounds; rough effort range (clearly a planning estimate, not a promise). A percentage only if derived from enumerated corpus. Resume capsule: baseline commit / GitHub cutoff / completed / current conclusions / open rulings / next round / expected evidence sources.

## 17.4 Complete current `SPEC.md`
Emit the entire current spec after every round. Never just a diff or a pointer. Must be sufficient to resume without the prior conversation.

## 17.5 Questions requiring human ruling
Only genuine product questions archaeology cannot resolve, in Finding/Ambiguity/Consequence/Assessment/Options format. If none, say so.

## 17.6 Live review (environment delta)
The checkpoint may be conducted live with the owner: present `SPEC.md` + delta + open questions, let the owner steer coverage and resolve rulings. `SPEC.md` remains self-contained for resumability regardless.

---

# 18. Resumption and handoff (unchanged)

Research must be resumable by another agent without hidden conversational state. During research `SPEC.md` carries a non-normative **Research State** appendix recording: baseline commit; GitHub cutoff; current round; corpus counts; completed evidence ranges; active behavioral questions; unresolved contradictions; requirements awaiting corroboration; open human rulings; post-baseline delta queue; exact next recommended work; **and a pointer to the Human Intent Timeline file (scaffolding, kept outside `SPEC.md`)**. A replacement agent reads the complete current `SPEC.md`, verifies the baseline, verifies counters, inspects open rulings, and continues from the recorded cursor. Do not restart from scratch. Remove the appendix when research is complete.

---

# 19. `SPEC.md` drafting standard (unchanged)

Behavioral, falsifiable, implementation-neutral, understandable to a Nostr developer unfamiliar with old NMP, precise about ownership and failure/restart behavior, aggressive about implementation freedom. No vague adjectives — spell out consequences. Not "queries are reactive" but what changes are observed and what the app receives. Not "offline-first" but what's readable offline, whether local queries run without relays, how network results merge after reconnect, what survives restart, what may stay stale, what publication obligations survive, what consistency is guaranteed. Not "publications are reliable" but the exact acceptance/persistence/retry/failure/delivery semantics.

---

# 20. Phase 7 — Final closure audit (unchanged)

Exhaustive audit against the initial corpus. Require objective accounting for: BDD/acceptance scenarios; application-facing docs/examples; behavioral tests; public application-facing surfaces; persisted state families; identified owner rulings (GitHub **and** transcripts); important historical pivots/reverts; cross-SDK contracts; unresolved product decisions. Every material evidence item has a disposition. No subjective "all major areas appear covered."

---

# 21. Final destructive review (unchanged)

## 21.1 Requirement deletion review — attempt to remove each; identify what fails; could the app/optional NIP layer own it; find speculative scenarios; check whether it compensates for another questionable requirement; narrow maximally; verify evidence.
## 21.2 Contradiction review — incompatible requirements/lifecycle/restart semantics, divergent historical rulings, hidden ordering assumptions. Resolve or mark.
## 21.3 Scope review — remove app policy, UI, signer, or protocol-specific convenience behavior that doesn't belong in generic NMP.
## 21.4 Terminology review — replace unexplained NMP nouns / implementation vocabulary / unnecessary abstraction with plain Nostr/software language.
## 21.5 Architecture-leak review — justify or remove every crate/module/reducer/actor/database/type/state-machine/queue reference.
## 21.6 Falsifiability review — every normative `MUST` supports a plausible acceptance test or falsifier.
## 21.7 End-to-end review — repeat important cross-cutting journeys against the final set; verify no hidden architecture assumption is needed to explain expected behavior.

---

# 22. Completion gate (unchanged)

Research is complete only when: every BDD/acceptance scenario classified; every application-facing doc/example group reviewed; every relevant behavioral test has a disposition; every meaningful public application-facing capability has a disposition; every significant persisted state family investigated for its behavioral reason; identified material human rulings (GitHub and transcript) incorporated/superseded/unresolved; identified major pivots/reverts/deletions examined; cross-SDK behavioral contracts reviewed; every surviving major requirement has evidence; every normative `MUST` has falsifiable acceptance criteria; failure and restart semantics explicit wherever materially relevant; ownership boundaries explicit; no surviving requirement exists solely because current machinery exists; speculative extensibility removed unless justified; important rejected premises represented as explicit non-requirements or tombstones where recurrence risk is high; final terminology needs no old-NMP knowledge; final taxonomy reflects product concepts not repository layout; no unexplained contradictions; no silently unresolved material product decisions; Research State appendix removed.

Final test: *Could an engineer who has never seen old NMP build a substantially smaller, architecturally different system that still satisfies every real application promise and every carefully settled correctness invariant?* If not, the spec is still contaminated by the current implementation.

---

# 23. Expected research shape (modified)

Because of transcript priming + parallelization + live owner checkpoints, the original 6–10 long autonomous rounds collapse toward:

1. **baseline + transcript priming + corpus census** (parallel census; Human Intent Timeline);
2. **behavioral seed v0** (timeline + BDD/docs) → **owner quick-review pass #1** (redirect);
3. targeted corroborating rounds (tests + public surfaces), parallelized by behavioral family, with owner checkpoints;
4. requirement-driven archaeology rounds for disputed/evolved behavior, parallelized, escalating rulings live;
5. reverse completeness sweep + owner coverage checkpoint;
6. end-to-end falsification + destructive pruning;
7. closure audit + final destructive review.

This is a planning assumption, not a fixed sequence; discoveries merge/split/reorder rounds. The process finishes when the completion gate is satisfied.

---

# 24. Prohibited shortcuts (unchanged + environment additions)

Do not: begin implementation; design the new crate architecture; use the repo tree as the spec outline; start by deeply understanding current internals; assume every BDD scenario is valid; assume every passing test belongs in the rewrite; assume every public API must survive; preserve sophisticated terminology without proving the concept; infer product intent from implementation momentum; preserve a mechanism because removing it looks difficult; silently resolve ambiguous product intent; turn one optional NIP implementation into a generic framework requirement; invent extensibility requirements; assume source compatibility; assume arbitrary runtime configurability is useful; defer every human question until the end; ask the human questions further archaeology could answer; emit partial `SPEC.md` patches; claim completion using subjective coverage language; produce architecture recommendations instead of requirements.

**Environment additions — also do not:**

* treat agent reasoning inside transcripts as human intent (only `#### user` blocks / owner messages are A0);
* hand-parse raw `~/.claude` / `~/.codex` JSONL when `pc recall dump` / `pc recall ask` already clean, scope, and cite it;
* dump raw transcripts into the lead's context — always distill via subagents (or use `pc recall ask --brief` for a cited answer instead of reading the dump);
* let a subagent assign a contested disposition — the lead synthesizes and decides;
* manufacture parallel work that overlaps and duplicates;
* trust a subagent finding without spot-checking it against the cited source (transcript quotes are checkable via the line pointer pc emits);
* skip the behavior-first framing when dispatching subagents ("understand crate X" is forbidden as a subagent prompt);
* mine transcripts for what the agent concluded rather than for what the owner said;
* promote a transcript statement to a requirement without checking for a later correction that reversed it;
* trust a `pc recall dump` `#### assistant` block as owner intent — assistant text is Class E until corroborated.