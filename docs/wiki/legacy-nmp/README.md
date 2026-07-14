# Legacy NMP evidence archive

This directory preserves recovered material from legacy NMP conversations. It is
an **evidence archive, not current NMP documentation**. Records may describe
rejected proposals, superseded mechanisms, transient tasks, app-specific work,
or positions that conflict with NMP v2.

For current authority, read the repository `README.md`, `docs/VISION.md`,
`docs/design-record.md`, the focused documents under `docs/design/`,
`docs/bug-class-ledger.md`, `docs/known-gaps.md`, and open GitHub issues.

## Corpus accounting

The canonical extraction contains exactly **3,189 records**:

| Extraction disposition | Records |
|---|---:|
| Ready | 1,145 |
| Clarification | 1,527 |
| Context | 394 |
| Historical | 123 |
| **Total** | **3,189** |

The first publication pass distributed only part of that corpus:

| First-pass carrier | Records |
|---|---:|
| Atomic GitHub issues #244-#409 | 345 |
| Clarification buckets #412-#437 | 699 |
| The 15 files now under `records/` | 249 |
| Not given a committed or GitHub carrier by that pass | 1,896 |
| **Total** | **3,189** |

PR #438 reported that it committed 271 historical/context records. That claim
was incorrect: both the `record_count` frontmatter and the `### LNK-...` record
headings in its 15 files sum to **249** (199 context and 50 historical). The 271
figure is not an alternate corpus boundary.

## Evidence and authorship safety

The text in `records/` is preserved as recovered evidence. Its presence does
not make it an accepted requirement, and a quote appearing in a user-role
message does not by itself prove that the user authored it. Such messages can
contain pasted agent recommendations, reports, attachments, logs, tool output,
or third-party text.

In particular, the legacy `authority: "direct-user-evidence"` frontmatter is
an extractor-era assertion, not a verified authorship guarantee. Use the
catalog's carrier/provenance fields and inspect the surrounding primary source
before describing wording as the user's own. Preserve literal spelling and
typos when quoting verified source; do not silently turn supplied material into
a user instruction.

## How to use the catalog and action map

[`catalog.jsonl`](catalog.jsonl) is issue #200's exhaustive disposition ledger: every one
of the 3,189 source records must occur exactly once and retain a pointer to its
primary evidence. The action map is the translation layer from that evidence to
NMP v2: it identifies the current invariant, implementation owner, existing
issue, historical rationale, superseded mechanism, or genuinely unresolved
question for each relevant teaching.

Use those artifacts to locate and explain evidence, not as a second
specification. Implement only from the current owning document or GitHub issue.
If legacy evidence reveals a missing or conflicting current rule, resolve the
source context, then update the current owner through the issue-first workflow.
Do not edit a legacy record to make history agree with the present.

Each JSONL row contains the canonical source metadata, exact evidence objects,
carrier partition, provenance safety status, and reviewed reconciliation. The
reconciliation class vocabularies are named because the atomic-issue,
historical-record, and final source-bank reviews asked different questions; the
catalog does not flatten those distinctions after the fact. `raw_context_checked`
records whether surrounding conversation was inspected.

One source record contained a literal Nostr secret key. The catalog replaces
that token with `[REDACTED_NSEC]` wherever it recurred and retains the original
token hash, whole-quote hash, offsets, and source-message hash. All other source
metadata and evidence remain exact. No row infers authorship merely from a
`user` message role; `direct_human` is retained as extractor provenance, not
upgraded into verified authorship.

Regenerate and verify the catalog against the private canonical extraction and
a live issue snapshot with:

```sh
python3 scripts/build-legacy-catalog.py \
  --source /path/to/a/final/issues.json \
  --live-issues /path/to/nmp-live-issues.json \
  --ledgers /path/to/completed-reconciliation-ledgers
python3 scripts/verify-legacy-catalog.py \
  --source /path/to/a/final/issues.json \
  --live-issues /path/to/nmp-live-issues.json
```

The verifier checks exact source-ID set equality, exact evidence after the
declared secret redaction, schema fields, valid reconciliation classes,
committed record headings, disjoint carriers, and the
345/699/249/1,896 partition.

A clean clone does not contain the private `a/final/issues.json` extraction or
the reconciliation ledgers. Its self-contained check is therefore:

```sh
python3 scripts/verify-legacy-catalog.py --self-contained
```

That mode still proves 3,189 unique sorted IDs, required source/evidence and
provenance fields, safe redaction metadata, valid reconciliation classes,
source-disposition counts, archive-heading equality, and the exact disjoint
carrier partition. When a canonical source is passed explicitly or discovered
through `NMP_LEGACY_SOURCE`, the verifier additionally performs the byte-level
source/evidence comparison. Rebuilding the catalog itself always requires the
canonical extraction, a live GitHub issue snapshot, and the completed review
ledgers shown above.

## Archived record groups

The 15 first-pass topic files are under [`records/`](records/). Their grouping
is for browsing only; topic placement and extractor disposition do not confer
authority.
