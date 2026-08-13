# Test placement

Place proof with the narrowest stable contract owner, not the edited crate.
Feature files follow behavioral domains; tests follow ownership and proof type.

## Decision table

| Claim | Place |
|---|---|
| Local value, parser, codec, transition | Owning-crate unit/table test |
| Broad invariant or operation sequence | Owning-crate property/model/differential test |
| One crate plus real collaborators | That crate's integration tests |
| Combined Rust product promise | `crates/nmp/tests/` through `nmp` |
| Readable cross-layer capstone | Canonical feature plus `@acceptance` when justified |
| FFI/platform behavior | Shared parity plus native Swift/Kotlin tests |
| Public provider/network compatibility | Opt-in live probe |

Do not enumerate broad state spaces in Gherkin or use public infrastructure as
the sole correctness proof.

## Fixture boundary

Setup may provide stores, clocks, scripted relays, identities, network policy,
faults, and sanctioned test constructors. It must not perform the behavior or
inspect private state as proof.

For discovery, seed the protocol fact and observe contacts/results. Do not
insert or inspect the resolved route directly.

## Structural proof and capstone

For important public behavior, use both when each proves something distinct:

1. mechanism-level property/model/integration proof;
2. one facade-level consequence.

Neither substitutes for the other. Avoid permanent Rust, shell, and Cucumber
copies of the same path.

## Avoid

- Test placement based only on the changed crate.
- Feature files mirroring crate structure.
- Cucumber for every invariant.
- One lucky end-to-end schedule as structural proof.
- Private table assertions for a facade promise.
- New test crates when an owning integration target suffices.

Before adding a target, state the contract no existing owner can prove and why
the target behaves as a consumer rather than a privileged bypass.
