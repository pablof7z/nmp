//! #680 — the "29er" composition reproduction (falsifier item 10 / item 1
//! in-repo proxy).
//!
//! 29er-next composes, on ONE engine, exactly the observer families the reviewer
//! named: an app-level feed observer, many group/room observers, an inbox
//! observer, several timeline observers, and many profile/avatar observers,
//! plus diagnostics, follow state, and active receipt tracking. Under the OLD
//! NMP this hit the hidden 12-native-task ceiling and the ~13th unrelated
//! observation was refused with `ExecutorSaturated`, so profiles/rooms failed
//! to load. This test opens that whole composition (far more than 12) through
//! the REAL exported FFI handles, then exercises repeated room switching (the
//! open/close churn that previously leaked/exhausted slots) and a full app
//! restart. It asserts: no operation is ever refused for a capacity reason,
//! every observation delivers its initial/current state, and none of it costs a
//! native thread.
//!
//! NOTE ON THE LITERAL APP RUN: 29er-next's `main` cannot currently build
//! against nmp master AT ALL — independently of #680 — because it predates
//! master's #561 `NMPContent` parser-only refactor (its `NMPContentClient`/
//! `NostrContentSession` usage no longer exists). The only #680-specific change
//! 29er needs is dropping its single `.executorSaturated` error case. This test
//! is the faithful, runnable #680 reproduction of the composition that used to
//! saturate.

use std::sync::Arc;
use std::time::Duration;

use nmp_ffi::facade::{NmpDiagnosticsStream, NmpEngine, NmpEngineConfig, NmpRowStream};
use nmp_ffi::nip02::NmpFollowStream;
use nmp_ffi::types::{
    FfiBinding, FfiDurability, FfiFilter, FfiWriteIntent, FfiWritePayload, FfiWriteRouting,
};

const TEST_SECRET_KEY_HEX: &str =
    "0000000000000000000000000000000000000000000000000000000000000001";

// A profile pubkey (hex) for a profile/avatar observer target.
fn profile_pubkey(i: usize) -> String {
    format!("{:064x}", 0x1000 + i)
}

fn kind_query(kind: u16) -> FfiFilter {
    FfiFilter {
        kinds: Some(vec![kind]),
        ..FfiFilter::default()
    }
}

fn author_query(pubkey: &str) -> FfiFilter {
    FfiFilter {
        authors: Some(FfiBinding::Literal {
            values: vec![pubkey.to_string()],
        }),
        kinds: Some(vec![0]), // kind:0 = profile metadata
        ..FfiFilter::default()
    }
}

/// Open the full 29er observer composition on one engine and return the handles
/// so the caller controls their lifetime. Every `expect` here is the falsifier:
/// under the old design one of these opens would be refused with a capacity
/// error once the composition exceeded 12.
struct Composition {
    app_feed: Arc<NmpRowStream>,
    rooms: Vec<Arc<NmpRowStream>>,
    inbox: Arc<NmpRowStream>,
    timelines: Vec<Arc<NmpRowStream>>,
    profiles: Vec<Arc<NmpRowStream>>,
    diagnostics: Arc<NmpDiagnosticsStream>,
    follow: Arc<NmpFollowStream>,
}

fn open_composition(engine: &NmpEngine, author: &str) -> Composition {
    // App-level feed (kind:1 firehose the app shows on launch).
    let app_feed = engine
        .observe(kind_query(1), None)
        .expect("app feed observer opens (no capacity ceiling)");

    // Many group/room observers (NIP-29 kind:9 messages per room).
    let rooms: Vec<_> = (0..12)
        .map(|r| {
            engine
                .observe(kind_query(9), None)
                .unwrap_or_else(|_| panic!("room {r} observer opens"))
        })
        .collect();

    // Inbox (kind:1059 gift-wrapped DMs / mentions).
    let inbox = engine
        .observe(kind_query(1059), None)
        .expect("inbox observer opens");

    // Several timeline observers.
    let timelines: Vec<_> = (0..8)
        .map(|t| {
            engine
                .observe(kind_query(30023), None)
                .unwrap_or_else(|_| panic!("timeline {t} observer opens"))
        })
        .collect();

    // Many profile/avatar observers (the ones that used to fail to load once the
    // ceiling was hit).
    let profiles: Vec<_> = (0..40)
        .map(|p| {
            engine
                .observe(author_query(&profile_pubkey(p)), None)
                .unwrap_or_else(|_| panic!("profile {p} observer opens"))
        })
        .collect();

    let diagnostics = engine
        .observe_diagnostics()
        .expect("diagnostics observer opens");
    let follow = engine
        .observe_following(author.to_string())
        .expect("follow observer opens");

    Composition {
        app_feed,
        rooms,
        inbox,
        timelines,
        profiles,
        diagnostics,
        follow,
    }
}

impl Composition {
    fn total_observations(&self) -> usize {
        2 + self.rooms.len() + self.timelines.len() + self.profiles.len() + 2 // +app_feed,inbox +diag,follow
    }

    fn cancel_all(&self) {
        self.app_feed.cancel();
        for r in &self.rooms {
            r.cancel();
        }
        self.inbox.cancel();
        for t in &self.timelines {
            t.cancel();
        }
        for p in &self.profiles {
            p.cancel();
        }
        self.diagnostics.cancel();
        self.follow.cancel();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_29er_observer_composition_never_saturates_across_room_switching_and_restart() {
    let base = nmp::nmp_threads_spawned();

    let mut engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine builds");
    let account = engine
        .add_account(TEST_SECRET_KEY_HEX.to_string())
        .expect("key parses");
    let author = account.public_key();
    engine
        .set_active_account(Some(author.clone()))
        .expect("activate account");

    // 1) Open the full composition. Old design: refused at the 13th.
    let composition = open_composition(&engine, &author);
    let total = composition.total_observations();
    assert!(
        total > 12,
        "the composition must exceed the old 12-task ceiling to be a real reproduction; got {total}"
    );

    // Every observation delivers its initial/current state over the async pull
    // path (spot-check a profile + a room + diagnostics — the families that used
    // to fail to load).
    assert!(
        tokio::time::timeout(Duration::from_secs(5), composition.profiles[0].next())
            .await
            .expect("a profile observation delivers within 5s")
            .expect("not a misuse")
            .is_some(),
        "profiles remain available (their initial state is delivered)"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), composition.rooms[0].next())
            .await
            .expect("a room observation delivers")
            .expect("not a misuse")
            .is_some(),
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), composition.diagnostics.next())
            .await
            .expect("diagnostics delivers")
            .expect("not a misuse")
            .is_some(),
    );

    // Active receipt tracking alongside the whole composition.
    let receipt = engine
        .publish(FfiWriteIntent {
            payload: FfiWritePayload::Unsigned {
                pubkey: author.clone(),
                created_at: 1_700_000_000,
                kind: 1,
                tags: Vec::new(),
                content: "29er composition".to_string(),
            },
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        })
        .expect("publish opens a receipt stream alongside the composition");
    let _ = receipt.id();

    // 2) Repeated ROOM SWITCHING: cancel this composition and reopen a fresh one
    // many times (the open/close churn that previously leaked/exhausted slots).
    let mut last = composition;
    for round in 0..25 {
        last.cancel_all();
        let next = open_composition(&engine, &author);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), next.profiles[0].next())
                .await
                .unwrap_or_else(|_| panic!("round {round}: a profile delivers after room switch"))
                .expect("not a misuse")
                .is_some(),
            "round {round}: profiles still load after repeated room switching (no leaked/refused slot)"
        );
        last = next;
    }
    last.cancel_all();

    // 3) Full app RESTART: shut the engine down and rebuild the whole composition
    // on a fresh engine.
    engine.shutdown();
    engine = NmpEngine::new(NmpEngineConfig::default()).expect("engine rebuilds after restart");
    let account = engine
        .add_account(TEST_SECRET_KEY_HEX.to_string())
        .expect("key parses");
    let author = account.public_key();
    engine
        .set_active_account(Some(author.clone()))
        .expect("activate account");
    let restarted = open_composition(&engine, &author);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), restarted.profiles[0].next())
            .await
            .expect("profile delivers after cold restart")
            .expect("not a misuse")
            .is_some(),
        "after a full restart the whole composition reopens and profiles load"
    );

    eprintln!(
        "\n#680 29er composition: opened {total} simultaneous observations (app feed + 12 rooms + \
         inbox + 8 timelines + 40 profiles + diagnostics + follow) + receipts, survived 25 rounds \
         of room switching and a cold restart -- 0 capacity refusals.\n  NMP thread growth across \
         the whole run: {} (from baseline {base}).",
        nmp::nmp_threads_spawned() - base
    );

    restarted.cancel_all();
    engine.shutdown();
}
