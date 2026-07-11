//! [`BootstrapDirectory`] — the app-owned [`RelayDirectory`] this demo feeds
//! `EngineThread::spawn` with.
//!
//! `nmp_router::RelayDirectory` is a plain synchronous fact lookup (no
//! network, no `async`) that gets boxed into `EngineCore` exactly once, at
//! construction time (`nmp_engine::core::EngineCore::new(store, directory,
//! cap)`), and never replaced afterward. `nmp-router`'s own doc comment on
//! the trait says live NIP-65 backing was meant to "arrive in M3 behind the
//! SAME trait" — as of this build only `nmp_router::FixtureDirectory`
//! (fixture-only, no network) implements it anywhere in the workspace. So
//! for a real app to get real outbox routing, the app itself must resolve
//! NIP-65 write relays for every author it cares about *before* spawning
//! the engine, and hand the engine a directory that already knows the
//! answers. That one-shot resolution is `bootstrap::bootstrap` (this
//! crate); this module is just the resulting static snapshot.
//!
//! Note this means the demo's outbox routing for *content* (kind:1) is only
//! as fresh as the bootstrap snapshot: an author who changes relays (or a
//! newly-followed author added mid-run) will not get picked up until the
//! process is restarted. The engine's *own* live kind:3 discovery (routed
//! through the two indexer relays because kind:3 is a discovery kind) keeps
//! refreshing the active pubkey's contact list live — but nothing feeds a
//! newly-discovered follow's write relays back into this directory at
//! runtime, because there is no live channel to do so. That gap is exactly
//! the Handle-surface friction this build is meant to surface (see the
//! top-level report).

use std::collections::HashMap;

use nmp_router::{LanedRelay, PubkeyHex, RelayDirectory};
use nostr::RelayUrl;

/// A static, one-shot RelayDirectory snapshot: NIP-65 write relays resolved
/// once (via the bootstrap phase) for every author the demo currently
/// cares about, plus the two operator-hardcoded indexer relays.
pub struct BootstrapDirectory {
    write: HashMap<PubkeyHex, Vec<LanedRelay>>,
    indexers: Vec<RelayUrl>,
}

impl BootstrapDirectory {
    pub fn new(write: HashMap<PubkeyHex, Vec<LanedRelay>>, indexers: Vec<RelayUrl>) -> Self {
        Self { write, indexers }
    }
}

impl RelayDirectory for BootstrapDirectory {
    fn write_relays(&self, author: &PubkeyHex) -> Vec<LanedRelay> {
        self.write.get(author).cloned().unwrap_or_default()
    }

    fn extra_relays(&self, _author: &PubkeyHex) -> Vec<LanedRelay> {
        Vec::new()
    }

    fn indexers(&self) -> Vec<RelayUrl> {
        self.indexers.clone()
    }

    fn pinned_relays(&self, _atom: &nmp_grammar::ConcreteFilter) -> Vec<LanedRelay> {
        Vec::new()
    }
}
