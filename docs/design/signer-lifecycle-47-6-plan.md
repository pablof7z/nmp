# Signer lifecycle — implementation plan (#47 + #6)

- **Date:** 2026-07-11
- **Status:** Design / implementation plan. **No code.** Governs GitHub issues
  **#47** (default identity, per-write override, reattachment, platform vault
  providers) and **#6** (NIP-46 async signer: offline `AwaitingSigner`, exact
  response validation, independent retry ownership). This is step 4 of #43's
  recommended order (`#47/#6 land signer selection, reattachment, and
  providers`), after #2/#3/#23 land crash-safe `Accepted` + canonical pending
  rows.
- **Contract sources (authoritative, not re-litigated here):** #43 agreed
  contract; #47 / #6 required-contract clauses; `docs/design/durable-write-signing-and-retry.md`
  §3 (signer selection + reattachment), §4 (secret-material boundary), §6
  (retry ownership table); `docs/known-gaps.md` "Signer selection is globally
  coupled today" + "Secret zeroization and platform signer-provider boundary".
- **Coordination with #2/#3 (`docs/design/crashsafe-accepted-2-3-plan.md`,
  Fable GO):** #2/#3 build the **persistence substrate** — the `OUTBOX_INTENTS`
  journal already carries `expected_pubkey`, an opaque `signing_identity_ref`
  placeholder, and a `sig_state` that includes `AwaitingSigner(pubkey)`; its §4
  builds the *persistence field* + a *stub* "reattach triggers `RequestSign`"
  hook. **This frame builds the signer-selection / provider / async model those
  hooks reference.** It does **not** redefine the persistence — it gives the
  placeholders their meaning and their behaviour.

---

## 0. The gap, precisely (grounded in current code)

Signer selection today is **globally coupled to the read root**, and signer
absence is **terminal**, not durable-waiting:

1. **Selection = the global `active` pointer, chosen at sign time, not pinned at
   acceptance.** `SignerRegistry.active_signer()` (`runtime/mod.rs:193`) returns
   `signers.get(&self.active?)`. `Effect::RequestSign(id, unsigned)` looks that
   up *fresh at dispatch* (`runtime/mod.rs:555`). `active` is moved by
   `Cmd::Engine(EngineMsg::SetActivePubkey)` **together with the read root**
   (`runtime/mod.rs:467-472`, "read root and the active signing capability move
   TOGETHER"). So a `set_active_account` between accept and sign silently
   retargets a write to a different signer — exactly the coupling #43/#47
   forbid ("a later current-pubkey change cannot redirect the intent").
2. **No per-write override exists.** `WriteIntent { payload, durability,
   routing }` (`outbox/mod.rs:43`) carries no identity selector.
3. **Missing signer is terminal `Failed`, not durable `AwaitingSigner`.** When
   no signer is active/registered, `RequestSign` sends
   `SignerCompleted(id, Err(SignerError::Unavailable))` (`runtime/mod.rs:573`);
   `on_signer_completed`'s `Err` arm (`core/mod.rs:605`) turns **every**
   `SignerError` — `Rejected`, `Unavailable`, `Timeout` alike — into
   `WriteStatus::Failed`, dropping the pending write. **Confirmed:** the
   `WriteStatus::AwaitingCapability` variant exists (`outbox/mod.rs:101`, mirrored
   at `nmp-ffi/types.rs:298`) but **has no producer anywhere in the engine** —
   the offline/reattach path it was reserved for was never built.
4. **NIP-46 is a bare seam.** `nmp-signer/src/remote.rs`'s `RemoteSignerHandle`
   declares only `public_key()`, ships zero impls (M3 §7 non-goal). No async
   signer, no correlation, no AUTH, no offline handling.

The substrate this frame plugs into (preserve, do not rebuild): `SigningCapability`
(`sign(UnsignedEvent) -> SignerOp<SignedEvent>`, `nmp-signer/src/capability.rs`),
the `SignerOp::Ready | Pending(rx)` pollable thunk already driven **without a
poll loop** by the runtime's single blocking-recv thread (`runtime/mod.rs:560-571`,
D8-clean), the pubkey-keyed `SignerRegistry`, and #2/#3's `OUTBOX_INTENTS`
journal + `accept_write`/recovery doors.

---

## 1. Signer-selection model — pin at acceptance, select by pinned pubkey

The one structural move that decouples selection from the read root: **the
target identity is resolved and pinned at acceptance (`on_publish`), carried on
the intent, and the signer is selected by that pinned pubkey — never by the
global `active` pointer.**

### 1.1 The pinned author pubkey *is* the selection key

A NIP-01 `UnsignedEvent` already carries `pubkey` (the author). That pubkey **is**
the natural, already-pinned selection key: select the signer registered for
`unsigned.pubkey`, not for `registry.active`. This alone closes gap #0.1 —
selection stops depending on a mutable global.

- **Default:** `publish(draft)` — at acceptance NMP stamps `unsigned.pubkey =
  current_pubkey` (the read root's pubkey) if the draft did not already commit an
  author. `expected_pubkey = current_pubkey`.
- **Override:** `publish(draft, as: identityRef)` — NMP stamps `unsigned.pubkey =
  resolve(identityRef)` and `expected_pubkey = resolve(identityRef)`. This selects
  a podcast / disposable / secondary identity **without touching `active`** — the
  read root is unmoved (#43: "without changing `currentPubkey`").

`expected_pubkey` is the `#2/#3` journal field (`OUTBOX_INTENTS`) and is already
"the *real* pinned identity" per that plan's Fable-Q5. This frame gives it
selection meaning: at acceptance it is fixed; a later `set_active_account`
cannot change it because `RequestSign` no longer reads `active`.

### 1.2 Wiring

- `WriteIntent` gains `pub identity: Option<IdentityRef>` (default `None` =
  current-pubkey). `IdentityRef` is a thin, pubkey-resolving selector (§3, owner
  Q1) — **not** a signer object; apps never hand a signer to `publish`.
- `on_publish` (`core/mod.rs:540`) resolves `expected_pubkey` from `identity`
  (override) or the read-root pubkey (default), stamps the `Unsigned` template's
  author, and pins `expected_pubkey` into the journal via #2/#3's `accept_write`.
  For a `Signed` payload, `expected_pubkey = event.pubkey` (already committed;
  the existing verify at `core/mod.rs:553` stays).
- `Effect::RequestSign` grows the pinned target: `RequestSign(ReceiptId,
  UnsignedEvent, PublicKey)` (the pubkey is redundant with `unsigned.pubkey` but
  makes selection explicit and survives any future template abstraction).
- `SignerRegistry` gains `fn by_pubkey(&self, pk: &PublicKey) -> Option<&dyn
  SigningCapability>`; the `RequestSign` dispatch arm (`runtime/mod.rs:555`)
  calls `registry.by_pubkey(&target)` instead of `registry.active_signer()`.
  `active` remains — it is still the read root and still the source of the
  *default* pubkey — but it is no longer consulted for selection.

**Result:** `publish(draft)` uses the current-pubkey signer; `publish(draft,
as:)` overrides per-write; account switches never retarget an accepted intent.

---

## 2. `AwaitingSigner` as the resting state — the terminal/retryable split

Signer *absence or transient failure* must become durable `AwaitingSigner`, not
`Failed`. This is the second structural change, in `on_signer_completed` and the
"no signer registered" dispatch arm.

### 2.1 No signer registered for the pinned pubkey → `AwaitingSigner`, not `Unavailable→Failed`

When `registry.by_pubkey(&target)` is `None`, do **not** synthesize a signer
error. Instead route to a new `EngineMsg::SignerUnavailable(id, pubkey)` that
sets the intent's journal `sig_state = AwaitingSigner(pubkey)` (via #2/#3's
persistence hook), emits `WriteStatus::AwaitingCapability` (giving the orphaned
variant its first producer), and **leaves the pending row live and query-visible**
(#2/#3 already inserted it at accept). Nothing is dropped; nothing is retracted.

### 2.2 Terminal vs retryable `SignerError` (the core behavioural change)

`on_signer_completed`'s `Err` arm (`core/mod.rs:605`) currently collapses all
errors to `Failed`. Split it by a **closed** taxonomy (owner Q5):

| Outcome | Meaning | Action |
|---|---|---|
| `Ok(event)` | signed | validate exact frozen body/id/pubkey + `verify`, then `promote_signed` (**already #2/#3 §3.3 / U3** — this frame does not re-own it) |
| `Err(Rejected(msg))` | explicit denial (user declined at the bunker; signer refused) | **terminal pre-signature failure** → #2/#3 §3.1 compensation door (retract pending row, restore displaced predecessor, delete journal rows), `WriteStatus::Failed` |
| `Err(InvalidResponse(..))` (**new**) | bunker returned a mutated / mismatched / non-verifying event | **terminal protocol failure** → same compensation door (#6: "any mutation is terminal, retracts the still-pending row") |
| `Err(Unavailable)` / `Err(Timeout)` / `Err(Disconnected)` (**new**) | signer temporarily offline / dropped mid-request | **NOT terminal** → revert `sig_state → AwaitingSigner(pubkey)`, emit `WriteStatus::AwaitingCapability`, **do not consume the intent**, wait for reattach (#47/#6: "Missing NIP-46 connectivity is not failure") |

This is the crux of both issues: absence and transient loss are *resumable*;
only explicit denial or a corrupt/mismatched response is *terminal*.

### 2.3 Reattachment — event-driven rescan (no poll)

#2/#3 §4 stubs "`Cmd::AddSigner` and boot recovery rescan `OUTBOX_INTENTS` for
`AwaitingSigner(pk)`". This frame supplies the **matching + re-dispatch model**:

- `runtime`'s `Cmd::AddSigner` (`runtime/mod.rs:402`), after `registry.add`
  returns the newly-registered `pk`, sends `EngineMsg::SignerAttached(pk)`.
- `EngineCore::on_signer_attached(pk)` scans open intents whose `sig_state ==
  AwaitingSigner(pk)` and re-emits the ordinary `RequestSign(id, unsigned, pk)`
  path for each. Boot `recover_outbox` (#2/#3 §2.3) does the same for
  `AwaitingSigner` **and** in-flight `Pending` intents recovered from a crash
  (Fable R1: a lost in-flight sign re-requests; double-signing is id-stable and
  harmless).
- This is **entirely event-driven** — an attach *causes* the rescan. There is
  **no signer poll loop and no signer deadline**: `AwaitingSigner` has no
  timeout (offline waits indefinitely), so it never enters the deadline
  scheduler and cannot trigger the Fable-R2 busy-spin. D8-clean by construction.

---

## 3. Provider / vault seam — obligations in core, secrets in the SDK

#43/#47/§4: "core stores obligations, not raw secrets; platform SDKs provide
standard signer providers/vaults." Two layers, one Rust seam.

### 3.1 What "obligation, not secret" means precisely

- **In-memory registry** (`SignerRegistry`) holds live `SigningCapability`
  objects — a `LocalKeySigner` *does* hold `nostr::Keys` in memory, and must, to
  sign. That is not a durable secret; it is a runtime capability the app
  attaches. (The `nmp-signer` zeroize hardening from known-gaps "Secret
  zeroization" is a companion cleanup — flagged, not owned here.)
- **Durable journal** (`OUTBOX_INTENTS`) persists **only** `expected_pubkey` +
  an opaque `signing_identity_ref` — never a secret key, never a bunker
  connection secret (#6: "a stable provider/identity reference, not raw
  bunker/local secrets"). On restart the registry is **empty**; every open
  intent is `AwaitingSigner` until the app re-attaches its signers.

### 3.2 The Rust seam (`nmp-signer` + facade re-export)

```
// nmp-signer
/// Stable, non-secret reference to a signing identity, persisted in the
/// journal as the obligation. Selection is by pubkey (§1); this refines
/// WHICH arrangement is expected, for diagnostics + provider-targeted reattach.
pub struct SigningIdentityRef {
    pub pubkey: PublicKey,
    pub provider: ProviderRef,   // opaque SDK-owned string (e.g. "local", "nip46:<id>")
}

/// SDK-facing: materializes a live SigningCapability for an identity from
/// platform secure storage / a bunker connection. The SDK ships standard
/// impls; the app owns import/removal/backup/consent policy.
pub trait SignerProvider {
    fn resolve(&self, identity: &SigningIdentityRef)
        -> Option<Box<dyn SigningCapability + Send>>;
}
```

Core never calls `SignerProvider` directly — the **SDK** does, and feeds the
result to the existing `Handle::add_signer` (which triggers §2.3 rescan). This
keeps the engine's contract at the already-proven `add_signer` verb; providers
are an SDK-side convenience, not a new core dependency.

### 3.3 SDK provider layer (Swift/Kotlin)

- **Standard local-key provider** backed by iOS Keychain / Secure Enclave and
  Android Keystore — so ordinary apps don't hand-roll vault plumbing (§4). It
  reads the key on launch and calls `add_account`/`add_signer`.
- **NIP-46 provider** (§4) — constructs the §4 `Nip46Signer` from a stored,
  non-secret connection descriptor and calls `add_signer`.
- App owns: which identities exist, import/removal/backup UX, consent. NMP owns:
  the obligation, the wait, the reattach.

`signing_identity_ref` is **selection-neutral** (owner Q2): selection is by
`expected_pubkey`, and "an equivalent signer = same pubkey" (§4 disposable-key
clause). `provider` is a diagnostic + reattach hint, not a second selector.

---

## 4. NIP-46 async signer (#6)

A real `Nip46Signer` that implements the **same `SigningCapability`** and plugs
into the **same** selection/registry/AwaitingSigner path as `LocalKeySigner` —
no parallel machinery.

### 4.1 Shape

- `Nip46Signer::sign(unsigned)` returns `SignerOp::Pending(rx)`. The runtime's
  existing Pending arm (`runtime/mod.rs:560`) already spawns **one blocking recv
  thread** per pending op and forwards exactly one `SignerCompleted` — D8-clean,
  no poll loop, no engine-thread block. #6 reuses this verbatim.
- `public_key()` returns the remote signer's pubkey from the connection
  descriptor, so the adapter can be **registered even while the bunker is
  offline** — enabling §1 selection-by-pubkey and §2.1 `AwaitingSigner`.

### 4.2 What the adapter owns (and does not)

Per #6 + §6 retry table — **one correlated request per `sign`**:

- **Owns:** its signer-relay connection; the single NIP-46 (kind:24133) request
  correlated by request id; required NIP-46 AUTH challenge handling; **exact
  response validation** — the returned event is for *this* request, is authored
  by the expected pubkey, matches the frozen body/id, and verifies. A mismatch
  resolves the op as `Err(InvalidResponse)` (§2.2 terminal); a rejection at the
  bunker resolves as `Err(Rejected)`; a dropped connection resolves as
  `Err(Disconnected)` (§2.2 retryable → `AwaitingSigner`).
- **Does NOT own:** durable write retry (the outbox / #2/#3 owns per-`(intent,
  relay)` lanes), relay publication of the signed event (transport owns it), or
  the durable pending row. After the adapter yields a valid event, everything
  downstream (promote, route, publish, receipts) is the existing path.

### 4.3 Offline / delayed / restart — the #6 proof path

- Accept while bunker offline → §2.1 `AwaitingSigner`, pending row immediately
  query-visible (#2/#3), receipt reattachable.
- Restart → `recover_outbox` reloads the intent as `AwaitingSigner`; registry
  empty until the SDK's NIP-46 provider re-attaches the reconnected adapter.
- Reattach → §2.3 rescan re-emits `RequestSign` → adapter signs → validate →
  `promote_signed` → route → publish → per-relay receipt evidence.

### 4.4 No write-failing signer timeout (owner Q3)

`AwaitingSigner` is the **resting state for every absence**, so there is
deliberately **no timeout that fails a write** for a slow/offline bunker (§3
"Missing NIP-46 connectivity is not failure"). The write is already
query-visible and optimistic; it waits. The only terminal outcomes are explicit
`Rejected` and `InvalidResponse`. Consequently **no signer op enters the deadline
scheduler** — sidestepping the Fable-R2 spin hazard entirely. A dropped
in-flight connection resolves as retryable `Disconnected` → back to
`AwaitingSigner` (the adapter's own connection reconnect is a socket concern,
like transport reconnect; it does not consume the durable write).

**Dependencies:** `Nip46Signer` is a **thin adapter over rust-nostr's NIP-46**
(`nostr-connect` / `nostr::nips::nip46`) — no scratch crypto (memory rule).
Confirm the exact rust-nostr surface + add the dependency at build (owner Q4:
does the adapter reuse `nmp-transport`'s pool for the signer relay, or own an
independent connection? #6 says it "owns its signer-relay connection" — leaning
independent, so `nmp-signer` gains a transport dep or a minimal owned socket).

---

## 5. Collision-safe decomposition

Dependency order **S1 → S2 → {S3, S4} → S5**. **S1 and S2 are the `core/mod.rs`
serialization points** and edit the **same functions** #2/#3's U3/U4 rewrite
(`on_publish`, `on_signer_completed`, `RequestSign` shape, recovery). **This
frame must sequence *after* #2/#3 U3/U4 lands, or co-own that worktree** — flag
to the orchestrator against #52 (public write surface) and any `core/mod.rs`
work.

### S1 — Selection: pin at acceptance + select-by-pubkey (decouple from `active`)
**Files:** `nmp-engine/src/outbox/mod.rs` (`WriteIntent.identity: Option<IdentityRef>`;
`IdentityRef`), `nmp-engine/src/core/mod.rs` (`on_publish` resolves+pins
`expected_pubkey`, stamps `Unsigned` author; `RequestSign` grows the target
pubkey), `nmp-engine/src/runtime/mod.rs` (`SignerRegistry::by_pubkey`; dispatch
arm selects by pinned pubkey, not `active`). **CORE + OUTBOX SEAM.** **Depends
#2/#3 U3** (shares `on_publish` + the `expected_pubkey` journal field).
**Tests:** default → current-pubkey signer; override → secondary signer with
`active` unmoved (read root unchanged); `set_active_account` between accept and
sign does **not** retarget a pinned intent.

### S2 — `AwaitingSigner` resting state: terminal/retryable split + reattach rescan
**Files:** `nmp-engine/src/core/mod.rs` (`on_signer_completed` split per §2.2;
`EngineMsg::SignerUnavailable`/`SignerAttached`; `on_signer_attached` rescan),
`nmp-engine/src/runtime/mod.rs` (no-signer arm → `AwaitingCapability` not
`Failed`; `Cmd::AddSigner` emits `SignerAttached`). Extends `SignerError` (§2.2,
new `InvalidResponse`/`Disconnected`) in `nmp-signer/src/op.rs`. **CORE SEAM.**
**Depends S1 + #2/#3 U3/U4** (compensation door; `AwaitingSigner` persistence +
recovery). **Tests:** absent signer → `AwaitingCapability` + pending row still
visible (no `Failed`); `Rejected`/`InvalidResponse` → compensation retract +
predecessor restored; `Unavailable`/`Disconnected` → reverts to `AwaitingSigner`,
intent not consumed; `add_signer(matching pk)` → rescan re-emits `RequestSign` →
signs; account-change cannot redirect a waiting intent.

### S3 — Provider / vault seam (Rust trait + obligation ref)
**Files:** `nmp-signer/src/` (new `SignerProvider`, `SigningIdentityRef`,
`ProviderRef`), facade re-export (`crates/nmp` — coordinate with #52). Gives
#2/#3's opaque `signing_identity_ref` its minimal meaning. **Low core-seam** —
parallel with S4. **Tests:** provider resolves a `SigningCapability` from an
identity ref; journal round-trips `signing_identity_ref` with no secret bytes;
`recover_outbox` yields `AwaitingSigner` until a provider re-attaches.

### S4 — NIP-46 adapter (#6)
**Files:** `nmp-signer/src/remote.rs` → real `Nip46Signer: SigningCapability`
(replaces the bare `RemoteSignerHandle` seam), correlation + AUTH + exact
response validation; `nmp-signer/Cargo.toml` (add rust-nostr NIP-46; owner Q4
transport). **Depends S1/S2** (selection + `AwaitingSigner`/error taxonomy).
**Tests (headless):** `sign` returns `Pending`; a correlated response verifies +
promotes; a mutated response → `InvalidResponse` (terminal, retracts); a bunker
rejection → `Rejected`; a dropped connection → `Disconnected` → `AwaitingSigner`.

### S5 — SDK providers + delayed-bunker falsifier (cross-surface proof)
**Files:** Swift/Kotlin standard local (Keychain/Keystore) + NIP-46 providers;
the #6 end-to-end falsifier + #47 default/override + restart-reattach proofs
across Rust/FFI/Swift/Kotlin. **Depends S1–S4 + #52 facade write surface.**
**Tests:** #6 proof — accept offline, observe pending row immediately, restart,
reattach bunker, sign, publish, per-relay evidence; reject/mutate → correct
pre-signature retraction incl. a replaceable predecessor; #47 default + override
paths proven on every surface; memory-only disposable key lost → intent stays
`AwaitingSigner`, never silently re-authored.

### Facade coordination (#52)
`WriteIntent.identity` + `publish(draft, as: identityRef)` are a **public
write-surface change** — the facade projection (`publish` overload / an
`as`-parameter) is #52's serialized surface work. `add_signer` and `add_account`
already exist (`canonical-facade-52-plan.md` §A). S5's Swift/Kotlin override path
threads through #52. Sequence S1's `WriteIntent` change with #52's `publish`
surface; do not add a second public publish entry point.

---

## 6. Owner questions (flagged, not invented around)

1. **`IdentityRef` shape.** Recommend a thin **pubkey-resolving** selector (the
   author pubkey is the natural, already-pinned selection key; `publish(draft,
   as:)` stamps `unsigned.pubkey`), *not* a passed signer object. Confirm apps
   never hand a signer to `publish` and the override is expressed as an identity
   reference resolved to a pubkey at acceptance.
2. **`signing_identity_ref` beyond `expected_pubkey`.** Recommend selection stays
   **by pubkey** ("equivalent signer = same pubkey", §4 disposable clause) and
   `signing_identity_ref.provider` is an opaque diagnostic + reattach hint only —
   not a second selector. Confirm no scenario requires two *distinct*
   arrangements for the *same* pubkey to be selected between at acceptance.
3. **No write-failing signer timeout.** Recommend `AwaitingSigner` be the resting
   state for every absence (offline/slow bunker never fails a write; only
   explicit `Rejected` / `InvalidResponse` are terminal), so **no signer op
   enters the deadline scheduler** (avoids the Fable-R2 spin). Confirm there is
   no product requirement for a bounded "give up waiting for a signer" timeout.
4. **NIP-46 signer-relay connection ownership.** #6 says the adapter "owns its
   signer-relay connection" — leaning an **independent** connection (adapter owns
   it), so `nmp-signer` gains a transport/socket dependency rather than sharing
   `nmp-transport`'s content-relay pool. Confirm, and confirm the exact rust-nostr
   NIP-46 (`nostr-connect`) surface to adapt thinly.
5. **`SignerError` closed taxonomy.** Recommend extending to a closed set that
   makes the terminal/retryable split (§2.2) explicit: `Rejected` +
   `InvalidResponse` = terminal; `Unavailable` / `Timeout` / `Disconnected` =
   retryable → `AwaitingSigner`. Confirm the exact variant set (and whether
   `Timeout` should exist at all given Q3).

---

## 7. What this frame explicitly does NOT build

- The `OUTBOX_INTENTS` journal, `accept_write`, `promote_signed`, the
  compensation door, `recover_outbox`, and the `AwaitingSigner` *persistence
  field* — all **#2/#3** (this frame consumes them).
- Exact frozen-body/id/pubkey validation + `verify` before promotion — **#2/#3
  U3** (this frame adds only the adapter-level correlation validation feeding it,
  and the terminal-vs-retryable *routing* of the result).
- Durable per-`(intent, relay)` retry, backoff, `OutcomeUnknown` policy, and the
  deadline-scheduler fold — the **#2/#3 retry-owner follow-up** (Fable R2).
- `nmp-signer` secret zeroization hardening — companion cleanup under known-gaps
  "Secret zeroization"; flagged, coordinate, not owned here.

---

## Fable checkpoint (verdict)

- **Date:** 2026-07-11. Reviewer: Fable (delegated design checkpoint; decisions
  below are final calls, not questions back to the owner).
- **Verdict: GO, with required changes** (R1–R5 below). No load-bearing flaw.
  Both grounding gaps in §0 were re-verified against the working tree, and the
  plan's structural moves (pin-at-acceptance, select-by-pinned-pubkey,
  `AwaitingSigner` resting state, event-driven reattach) each close a confirmed
  gap without new machinery.

### Grounding re-verified in code

- **Gap (a) — selection reads the mutable global at sign time: CONFIRMED.**
  `SignerRegistry.active_signer()` returns `signers.get(&self.active?)`
  (`runtime/mod.rs:193-197`); the `Effect::RequestSign` dispatch arm looks it up
  *fresh at dispatch* with an explicit comment that this is so "a
  `set_active_account` switch is observed by the very next publish"
  (`runtime/mod.rs:~555`); `Cmd::Engine(EngineMsg::SetActivePubkey)` moves
  `registry.set_active(pk)` together with the read root ("move TOGETHER",
  `runtime/mod.rs:~467`). An account switch between accept and sign therefore
  retargets a pending write today — exactly the coupling #43/#47 forbid.
- **Gap (b) — `AwaitingCapability` has zero producers; no-signer collapses to
  `Failed`: CONFIRMED.** Workspace grep finds `AwaitingCapability` only at its
  definition (`outbox/mod.rs:101`) and the FFI mirror
  (`nmp-ffi/types.rs:298`, `convert.rs:319`) — no engine site ever emits it. The
  no-signer dispatch arm synthesizes `SignerCompleted(id,
  Err(SignerError::Unavailable))` (`runtime/mod.rs:~573`), and
  `on_signer_completed`'s `Err` arm removes the pending write and emits
  `WriteStatus::Failed` for **every** `SignerError` (`core/mod.rs:~605`).
- **Select-by-pinned-pubkey genuinely closes (a):** `expected_pubkey` is pinned
  in the one crash-atomic accept commit (#2/#3 §2.1), `RequestSign` carries the
  target, dispatch selects `registry.by_pubkey(&target)`. `active` is never
  consulted for selection; a later `SetActivePubkey` moves only the read root +
  the *default* for future accepts. Closed.
- **Reattach is event-driven, no R2 spin:** the rescan is caused by
  `Cmd::AddSigner → SignerAttached(pk)` (plus one boot rescan in
  `recover_outbox` replay). No timer is armed, `AwaitingSigner` never enters
  `next_deadline()` — the R2 hazard (a fired deadline nothing consumes re-arms
  as already-past → busy spin in the `recv_timeout` driver) is impossible here
  *by absence of any deadline*, not by careful clearing. D8-clean.
- **Journal never holds secrets:** `OUTBOX_INTENTS` carries `expected_pubkey` +
  opaque `signing_identity_ref` only; live capabilities exist solely in the
  in-memory registry; registry is empty at boot → everything recovers as
  `AwaitingSigner` until the app re-attaches. Consistent with crashsafe-plan
  Fable-Q5. (See R2 for the one opacity rule this needs.)
- **NIP-46 `Pending` reuses the existing thread:** the runtime's Pending arm
  (`runtime/mod.rs:~560`) already spawns one blocking-recv thread per op and
  forwards exactly one `SignerCompleted`. `Nip46Signer::sign` returning
  `SignerOp::Pending(rx)` plugs in verbatim — no new thread model, no poll.

### The five decisions

**Q1 — `IdentityRef` = a thin pubkey-resolving selector: CONFIRMED.** Apps
never hand a signer object to `publish`. Grounds: capabilities are *attached*
(`add_signer`) and live only in the registry; a per-write signer object would
create a second attach path and an unjournalable obligation (a live object
cannot be persisted; the pinned pubkey can). The author pubkey on the NIP-01
`UnsignedEvent` is the natural, already-pinned key. Shape: a closed enum,
initially `IdentityRef::Pubkey(PublicKey)` — resolved to a concrete pubkey at
acceptance, journal stores only the resolved `expected_pubkey` + ref. Extending
the enum later (e.g. a provider-scoped selector) is additive and stays closed.

**Q2 — selection stays by-pubkey; `signing_identity_ref` is diagnostic-only:
CONFIRMED, with a structural observation that settles it.** The registry is
**one capability per pubkey by construction** — `signers:
HashMap<PublicKey, Box<dyn SigningCapability>>`, and `add` *replaces* any prior
capability for the same key (`runtime/mod.rs:~168-180`). So "two distinct
arrangements for the same pubkey, selected between at acceptance" is not a
state the engine can even hold; the *app* chooses which arrangement is attached,
and NMP never substitutes one arrangement for another on its own. Cryptographic
equivalence is exactly pubkey equivalence: promotion validates the frozen
body/id/pubkey and `verify`s — arrangement-blind by design (#47's "equivalent
signer = same pubkey" disposable clause). Codex's channel caution ("do not
silently switch hardware/remote/approval arrangements") is honored precisely
because switching requires an explicit app `add_signer` act; per R3 that act is
recorded against the ref in diagnostics, never performed by NMP. `provider`
remains a diagnostic + provider-targeted reattach hint, not a second selector.

**Q3 — no bounded "give up waiting for a signer" timeout: CONFIRMED.** Absence
is the resting state. This cannot strand intents: the pending row is
query-visible from acceptance (#2/#3), the receipt is reattachable across
restart, and `EngineMsg::CancelWrite` (#2/#3 U3) is the explicit exit — the app
always holds the cancel verb, so "waits indefinitely" means "waits until the
*user's* decision", which is #6's contract verbatim ("until a matching provider
reconnects or the app cancels it"). The only time-based terminal that can touch
a waiting intent is NIP-40 protocol expiry, which is already owned by the
existing expiry deadline and is a property of the event body, not of signer
availability. Consequence stands: **no signer op ever enters the deadline
scheduler** — the R2 spin class is structurally unreachable. (Codex concurs:
"a timer would turn capability absence into policy and add wake churn.")

**Q4 — NIP-46 connection ownership + rust-nostr surface: DECIDED — the adapter
owns an independent connection, built on `nmp-transport::Pool` + the `nostr`
crate's `nip46` feature. Do NOT use the `nostr-connect` client crate.**
- **Not `nostr-connect`:** it is tokio/async (nostr-relay-pool underneath).
  This workspace's production graph is deliberately runtime-free — the pool is
  mio + tungstenite worker threads, and tokio appears **only** in
  dev-dependencies with an explicit D8 comment (`nmp-transport/Cargo.toml`).
  Pulling `nostr-connect` into `nmp-signer` would impose tokio on every
  embedder — a doctrine break, not a convenience.
- **Not the engine's pool *instance*:** the engine pool's `PoolEventSink`
  feeds `EngineMsg`/ingest — kind:24133 signer traffic must never enter the
  store/resolver path, and the signer relay's lifecycle (connect on demand,
  AUTH as the *client* key, drop when idle) is the adapter's, not the content
  router's. #6 says the adapter "owns its signer-relay connection" — that is an
  ownership statement about instance + lifecycle, and it holds.
- **Yes to reusing the pool *type*:** `Pool::new(cfg, sink)` is a cheap,
  self-contained facade (`Arc<Mutex<PoolInner>>` + per-relay worker thread);
  `ensure_open`/`send`/`close`/`shutdown` and the sink's `PoolEvent::Frame`
  push-model are exactly the shape one correlated request/response needs. Two
  free wins verified in code: the pool **pre-classifies NIP-42 `AUTH` frames**
  (`RelayFrame::Auth`) — the adapter's required AUTH handling hangs off that —
  and the ingest verify gate (`pool/verify.rs`) is kind-blind schnorr
  verification, which bunker-signed kind:24133 responses pass. So: `nmp-signer`
  gains a `nmp-transport` dependency (a lower-layer dep, no cycle) and owns its
  own `Pool` instance per bunker connection. No hand-rolled socket.
- **Exact protocol surface (verified present in `nostr` 0.44.4 behind feature
  `nip46`):** `nostr::nips::nip46::{NostrConnectURI, NostrConnectRequest,
  NostrConnectMethod, NostrConnectMessage, NostrConnectResponse,
  ResponseResult}` — bunker/nostrconnect URI parsing (`relays()`,
  `remote_signer_public_key()`, `secret()`), request-id correlation
  (`NostrConnectMessage::request` / `.id()` / `.to_response(method)`),
  `ResponseResult::to_sign_event`, and `is_auth_url` for the NIP-46 AUTH-URL
  challenge. Payload encryption is NIP-44 via the already-enabled `nip44`
  feature. `nmp-signer/Cargo.toml`: add `"nip46"` to the existing `nostr`
  features — no new crate.

**Q5 — `SignerError` closed taxonomy: RATIFIED.** Final set:
`Rejected(String)` + `InvalidResponse(String)` = **terminal** (compensation
door, `Failed`); `Unavailable` + `Timeout` + `Disconnected` = **retryable**
(revert to `AwaitingSigner`, emit `AwaitingCapability`, intent not consumed).
**Keep `Timeout`**: the engine never times a signer (Q3), but the variant is
produced by `SignerOp::wait`'s `RecvTimeoutError::Timeout` mapping (a public
utility for direct embedders/tests) and is the honest word for an adapter that
bounds its *own* RPC internally. Its classification is what matters: retryable,
identical to `Unavailable` — a self-timing-out adapter can never fail a write,
only return it to rest. Update `SignerOp::wait`'s and the enum's doc comments
so no embedder mistakes `Timeout` for a terminal (folded into R4). The set
stays closed; `op.rs`'s "A3 may extend this closed set" note is superseded by
this ratified taxonomy.

### Required changes (builder must incorporate; none needs a redesign)

1. **R1 — pubkey-less capabilities must be a typed attach error, not a silent
   drop.** `SignerRegistry::add` currently *silently discards* a capability
   whose `public_key()` is `None` (returns `None`, nothing registered,
   `runtime/mod.rs:~176-180`). Under by-pubkey selection that becomes a
   footgun: the app believes it attached a signer; every matching intent waits
   forever. `Handle::add_signer` must surface a typed error for a pubkey-less
   capability, and the NIP-46 provider must resolve the user pubkey (from the
   `bunker://` descriptor, or by completing the connect handshake /
   `get_public_key` for `nostrconnect://` flows) **before** attaching.
2. **R2 — `ProviderRef` opacity rule.** The journal's provider string must
   never embed the bunker connection URI or token — `NostrConnectURI` can carry
   a `secret`, and the client transport key is a secret. `ProviderRef` is a
   vault/SDK lookup key only (e.g. `"nip46:<uuid>"`); the descriptor itself
   lives in the platform vault. State this on the `SigningIdentityRef` type.
3. **R3 — record arrangement changes in diagnostics.** When a signer attaches
   for a pubkey whose waiting intents carry a `signing_identity_ref.provider`
   that names a *different* provider, proceed (Q2: same pubkey = equivalent)
   but record the provider mismatch in the diagnostics stream (#51 owns the
   permanence). This is the cheap honesty codex's caution asks for, without
   making `provider` a selector.
4. **R4 — doc the taxonomy at the type.** The terminal-vs-retryable split
   (§2.2) must live as doc comments on `SignerError` and `SignerOp::wait`, not
   only in this plan — direct embedders see the type, not the plan.
5. **R5 — `Signed` payload + identity override conflict is a typed acceptance
   rejection.** `publish(signedEvent, as: X)` where `event.pubkey !=
   resolve(X)` must fail at acceptance (before `accept_write`, no journal
   residue) — never silently prefer either. The default (`as` absent) keeps
   §1.2's `expected_pubkey = event.pubkey`.

### Sequencing (my call — sets the build order)

- **S1/S2 land strictly after #2/#3 U3 and U4 respectively; no co-owned
  worktree.** S1 edits `on_publish` + the `RequestSign` shape and consumes the
  `expected_pubkey` journal field U3 creates → **S1 starts after U3 is on
  master**. S2 edits `on_signer_completed`, consumes the compensation door
  (U3) and the `AwaitingSigner` persistence + boot-recovery rescan stubs (U4)
  → **S2 starts after U4 is on master**. The crashsafe checkpoint already
  ruled U3/U4 the sole `core/mod.rs` writers of that frame; this frame's S1/S2
  become the sole `core/mod.rs` writers *after* it. Serial chain, no sharing.
- **S3 may start immediately** — it touches only `nmp-signer` (new trait +
  ref types), no core files. Its facade re-export waits for #52 Unit A.
- **S4 after S1/S2** (needs the extended `SignerError` and the selection
  path); its `nmp-signer`-internal protocol scaffolding (correlation,
  NIP-44 framing, URI parsing) may be developed in parallel but lands behind
  S2.
- **`WriteIntent.identity` + `publish(draft, as:)` is #52 surface work.** The
  facade overload, FFI projection, and Swift/Kotlin parity go through #52's
  serialized surface lane with synchronized falsifiers (#43 governance:
  cross-surface impact review, one publish entry point — no second publish
  verb). S1 lands the core `WriteIntent` field; the public projection ships
  with #52's surface PR, and S5's cross-surface proofs run last.
- **Full order:** #2/#3 U1/U2 → U3 → U4 → **S1 → S2** → {S3 (early-startable)
  ∥ S4} → S5 (after #52 surface). #52 A remains parallel-safe throughout
  (disjoint files) except the S3 facade re-export and the S1-adjacent publish
  overload noted above.

### Residual risk

- **Capability replace during an in-flight `Pending` op:** `add` replaces the
  registered capability while the old op's recv thread still lives; the old
  thread's eventual `SignerCompleted` still resolves the intent (id-stable,
  validated before promote) — harmless, but a duplicate approval prompt can
  reach a human at the bunker (already flagged in the crashsafe checkpoint's
  residuals; #47's provider correlation is the eventual home).
- **Unbounded `AwaitingSigner` accumulation** if an app never attaches or
  cancels: deliberate (the rows are visible, cancellable, and GC-claimed per
  crashsafe R5), but SDK providers should surface "N writes waiting for
  signer X" so the wait is never invisible. S5's providers own that surfacing.
- **`nostr` 0.44.4's nip46 module is client-complete** for this adapter's needs
  (verified above), but the NIP-46 AUTH-URL flow (`is_auth_url`) requires an
  app-facing hook to open the URL — an SDK-provider concern; keep it out of
  core.
- The interim state between S1 and S2 (selection fixed, but no-signer still
  `Failed`) is briefly *worse-looking* than S2's end state but no worse than
  today; acceptable inside one frame, do not release-cut between S1 and S2.
