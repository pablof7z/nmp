// Proof binary for the concurrent-worktree disk-cache proof
// (tools/bazel/proof/run_proof.sh). The script edits VALUE per worktree so
// each worktree's binary prints a distinct value; the shared `hex` dependency
// is identical across worktrees, so its compile action is served from the
// shared Bazel disk cache while each worktree's own `printer` binary is
// recompiled from its own (distinct) source -- proving no cross-worktree
// output contamination.
const VALUE: &str = "default";

fn main() {
    // Reference `hex` so the dependency is live (not dead-stripped): the shared
    // third-party compile action is what the cache-hit step observes.
    let _ = hex::encode([0u8, 1, 2, 3]);
    println!("{VALUE}");
}
