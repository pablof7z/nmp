//! #1802: `AuthorRouteProvider` is app-supplied code, invoked synchronously
//! on the reducer thread under a `RefCell` borrow. Before the guard, a
//! panic inside it unwound `engine_loop` itself -- killing the reducer
//! thread and leaving every outstanding `Handle` with a dead inbox. These
//! tests drive a real `EngineThread`, not just the `provider_reroot`
//! function directly, because "the panic was caught" is not the same claim
//! as "the engine thread is still alive and answering" -- only the second
//! is what an app actually needs.

use super::*;
use nmp_engine::core::{AuthorRouteUpdate, ObservationEvidence, ProviderReroot, RowDelta};
use nmp_grammar::{Binding, Demand, Filter, LiveQuery};
use nostr::Keys;
use std::collections::BTreeSet;
use std::time::Duration;

/// A provider that panics on the first call the loop makes into it,
/// exactly the failure #1802 describes: no `catch_unwind`, no timeout, no
/// capability token on this seam before the fix.
struct PanickingProvider;

impl AuthorRouteProvider for PanickingProvider {
    fn reroot(
        &mut self,
        _needs: BTreeSet<nostr::PublicKey>,
    ) -> (ProviderReroot, Vec<AuthorRouteUpdate>) {
        panic!("injected AuthorRouteProvider panic (#1802 falsifier)");
    }

    fn observe_rows(&mut self, _rows: &[RowDelta]) -> Vec<AuthorRouteUpdate> {
        unreachable!("this test's provider never opens an observation")
    }

    fn observe_evidence(&mut self, _evidence: &[ObservationEvidence]) -> Vec<AuthorRouteUpdate> {
        unreachable!("this test's provider never opens an observation")
    }
}

#[test]
fn a_panicking_author_route_provider_does_not_kill_the_engine_thread() {
    let (engine, handle) = EngineThread::spawn_with_runtime_config(
        RedbStore::temporary().expect("temporary Redb store"),
        8,
        PoolConfig::default(),
        RuntimeConfig::default(),
        Vec::new(),
        Some(Box::new(PanickingProvider)),
    )
    .expect("engine construction");

    // A `Subscribe` naming an author is the ordinary door that makes the
    // reducer discover an author-route need and dispatch
    // `Effect::AuthorRouteNeedsChanged` in the same turn -- which is where
    // the panicking provider gets called. The `Cmd::Subscribe` handler
    // replies to this call BEFORE dispatching that effect (lib.rs), so a
    // successful reply here proves nothing about panic containment by
    // itself; the probe below is the real assertion.
    let author = Keys::generate().public_key();
    let filter = Filter {
        kinds: Some(BTreeSet::from([1])),
        authors: Some(Binding::Literal(BTreeSet::from([author.to_hex()]))),
        ..Filter::default()
    };
    handle
        .subscribe(LiveQuery::single(
            Demand::author_outboxes(filter).expect("the selection binds `authors`"),
        ))
        .expect("the subscribe reply itself precedes effect dispatch");

    // The reducer processes `Cmd`s strictly in order off one inbox, so by
    // the time this second, unrelated round trip is served, the first
    // command's `AuthorRouteNeedsChanged` dispatch -- and the provider
    // panic inside it -- has already run to completion on the same
    // thread. If the panic were not caught, `engine_loop` would have
    // unwound and this call would never receive a reply.
    let (probe_tx, probe_rx) = mpsc::channel();
    let probe_handle = handle.clone();
    thread::spawn(move || {
        let result = probe_handle.subscribe(LiveQuery::single(Demand::public(Filter::default())));
        let _ = probe_tx.send(result.is_ok());
    });
    assert_eq!(
        probe_rx.recv_timeout(Duration::from_secs(5)),
        Ok(true),
        "the engine thread must still be alive and answering an ordinary \
         subscribe after the provider panicked"
    );

    handle.shutdown();
    engine.join();
}
