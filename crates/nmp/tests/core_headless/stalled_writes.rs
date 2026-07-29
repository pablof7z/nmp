//! The bounded, read-only stalled-write projection (#756/#968).
//!
//! These are the properties the wire tier cannot reach: a signer that never
//! answers, an active account that changes under a frozen obligation, more
//! stalled writes than any snapshot may carry, and a crash whose reopen must
//! reproduce the same descriptors from durable facts alone.
//!
//! Every assertion reads `EngineCore::diagnostics_snapshot` — the same door
//! the runtime pushes to observers — and nothing here reaches into reducer
//! internals, because the whole claim is that an app holding no receipt can
//! see this.

use super::*;

use nmp::mechanism::core::{StalledWrite, StalledWriteStage};
use nmp_router::{Lane, LanedRelay, LiveDirectory, RelayDirectory};

fn directory_knowing(author: &Keys, relay: &RelayUrl) -> LiveDirectory {
    let mut directory = LiveDirectory::builder()
        .indexers([RelayUrl::parse("wss://indexer.example").unwrap()])
        .build();
    directory.ingest_write_relays(
        author.public_key().to_hex(),
        vec![LanedRelay::new(relay.clone(), Lane::Nip65Write)],
    );
    directory
}

fn empty_directory() -> LiveDirectory {
    LiveDirectory::builder()
        .indexers([RelayUrl::parse("wss://indexer.example").unwrap()])
        .build()
}

fn stalled<S: EventStore>(core: &EngineCore<S>) -> Vec<StalledWrite> {
    core.diagnostics_snapshot().stalled_writes
}

/// Publish `builder` and hand its signer request back signed.
fn publish_signed<S: EventStore>(
    core: &mut EngineCore<S>,
    author: &Keys,
    builder: nmp_grammar::EventBuilder,
    routing: WriteRouting,
) -> ReceiptId {
    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(builder),
        durability: Durability::Durable,
        routing,
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, unsigned) = find_sign_request(&accepted);
    let signed = unsigned.sign_with_keys(author).unwrap();
    core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
    id
}

/// A write whose route resolved to nothing is exactly the population the
/// receipt cannot describe to anyone who is not holding it: the park's own
/// reason is what the global list carries, word for word.
#[test]
fn an_unroutable_write_is_listed_with_the_reason_its_receipt_was_parked_with() {
    let author = Keys::generate();
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(empty_directory()), 10);
    activate(&mut core, &author);

    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(1, "cold start")),
        durability: Durability::Durable,
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, unsigned) = find_sign_request(&accepted);
    let signed = unsigned.sign_with_keys(&author).unwrap();
    let effects = core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));

    let parked = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitReceipt(receipt, WriteStatus::AwaitingRoute { detail })
                if *receipt == id =>
            {
                Some(detail.clone())
            }
            _ => None,
        })
        .expect("a write with nothing to route to parks");

    let rows = stalled(&core);
    assert_eq!(
        rows.len(),
        1,
        "expected exactly one stalled write: {rows:?}"
    );
    assert_eq!(rows[0].stage, StalledWriteStage::Unroutable);
    assert_eq!(
        rows[0].detail, parked,
        "the global list carries the receipt's own recorded reason rather than a second, \
         separately-derived sentence that could drift from it"
    );
    let totals = core.diagnostics_snapshot().stalled_write_totals;
    assert_eq!(totals.unroutable, 1);
    assert_eq!(totals.unsignable, 0);
    assert_eq!(totals.undeliverable, 0);
    assert_eq!(totals.omitted_details, 0);
}

/// The identity half of the contract: a missing signer stays pinned to the
/// pubkey FROZEN at acceptance, and switching the active account underneath
/// it changes nothing about what the list says it is waiting for.
#[test]
fn an_unsignable_write_names_the_frozen_author_across_an_account_switch() {
    let author = Keys::generate();
    let someone_else = Keys::generate();
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(empty_directory()), 10);
    activate(&mut core, &author);

    let accepted = core.handle(EngineMsg::Publish(WriteIntent {
        payload: WritePayload::Event(draft(2, "nobody can sign this")),
        durability: Durability::Durable,
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    }));
    let (id, generation, _unsigned) = find_sign_request(&accepted);
    core.handle(EngineMsg::SignerUnavailable(id, generation));

    let before = stalled(&core);
    assert_eq!(before.len(), 1, "expected one stalled write: {before:?}");
    assert_eq!(before[0].stage, StalledWriteStage::Unsignable);
    assert!(
        before[0].detail.contains(&author.public_key().to_hex()),
        "the park names the capability it waits for: {:?}",
        before[0].detail
    );

    activate(&mut core, &someone_else);
    let after = stalled(&core);
    assert_eq!(
        before, after,
        "the mutable active account is never what a frozen obligation is waiting for"
    );
    assert!(
        !after[0]
            .detail
            .contains(&someone_else.public_key().to_hex()),
        "diagnostics must never report whoever is active now: {:?}",
        after[0].detail
    );
}

/// The `wss://non-existent.example` case, at the reducer: routing succeeded
/// perfectly and instantly, and it is delivery that never happens. The row
/// appears while nothing holds a session to the destination, and leaves the
/// moment one exists — without anything having asked it to.
#[test]
fn a_routed_write_is_undeliverable_only_while_no_destination_is_connected() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://non-existent.example").unwrap();
    let mut core = EngineCore::new(
        MemoryStore::new(),
        Box::new(directory_knowing(&author, &relay)),
        10,
    );
    activate(&mut core, &author);

    publish_signed(
        &mut core,
        &author,
        draft(3, "nowhere to land"),
        WriteRouting::Explicit(vec![relay.clone()]),
    );

    let rows = stalled(&core);
    assert_eq!(rows.len(), 1, "expected one stalled write: {rows:?}");
    assert_eq!(rows[0].stage, StalledWriteStage::Undeliverable);
    assert!(
        rows[0].detail.contains(relay.as_str()),
        "the reason names the destination nothing answers for: {:?}",
        rows[0].detail
    );

    let session = signer_session(&relay, author.public_key());
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        session.clone(),
    ));
    assert!(
        stalled(&core).is_empty(),
        "a destination this process holds a session to is progressing, not stuck: {:?}",
        stalled(&core)
    );

    core.handle(EngineMsg::RelayDisconnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        session,
        DisconnectReason::Error,
    ));
    assert_eq!(
        stalled(&core).len(),
        1,
        "and it returns when the session does not"
    );
}

/// More stalled writes than any snapshot may carry. Memory stays fixed, the
/// census stays exact, and the ordering is the documented display order
/// rather than whatever a hash map iterated.
#[test]
fn the_detail_window_is_bounded_while_the_census_stays_exact() {
    const PARKED: u64 = 200;
    let author = Keys::generate();
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(empty_directory()), 10);
    activate(&mut core, &author);

    for i in 0..PARKED {
        publish_signed(
            &mut core,
            &author,
            draft(1_000 + i, &format!("parked {i}")),
            WriteRouting::Auto,
        );
    }

    let snapshot = core.diagnostics_snapshot();
    let limit = snapshot.stalled_write_totals.detail_limit;
    assert!(limit > 0 && limit < PARKED, "the bound must actually bind");
    assert_eq!(
        u64::try_from(snapshot.stalled_writes.len()).unwrap(),
        limit,
        "the detail window is exactly the configured bound"
    );
    assert_eq!(
        snapshot.stalled_write_totals.unroutable, PARKED,
        "showing the first N as all writes is the failure this census exists to prevent"
    );
    assert_eq!(
        snapshot.stalled_write_totals.omitted_details,
        PARKED - limit,
        "the omission is exact, not a boolean 'there is more'"
    );

    // Deterministic across repeated reads of the same state -- a hash-map
    // iteration order would not be.
    let again = core.diagnostics_snapshot();
    assert_eq!(
        snapshot.stalled_writes, again.stalled_writes,
        "selection and ordering are a documented display priority, not map iteration"
    );
    let mut sorted = snapshot.stalled_writes.clone();
    sorted.sort_by(|a, b| {
        a.stage
            .cmp(&b.stage)
            .then(a.stalled_since.cmp(&b.stalled_since))
            .then_with(|| a.id.cmp(&b.id))
    });
    assert_eq!(snapshot.stalled_writes, sorted);
}

/// Two obligations for byte-identical events are two obligations, and the
/// descriptor must say so — otherwise an app watching a row leave the list
/// could watch the wrong one.
#[test]
fn two_receipts_for_the_same_bytes_get_distinct_descriptors() {
    let author = Keys::generate();
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(empty_directory()), 10);
    activate(&mut core, &author);

    for _ in 0..2 {
        let accepted = core.handle(EngineMsg::Publish(WriteIntent {
            payload: WritePayload::Event(draft(7, "the same bytes twice")),
            durability: Durability::Durable,
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        }));
        // The SECOND acceptance joins an already-signed row's owner set, so
        // it asks no signer -- it is a distinct obligation over identical
        // bytes, which is precisely the case this test exists for.
        if let Some((id, generation, unsigned)) = accepted.iter().find_map(|effect| match effect {
            Effect::RequestSign(id, generation, unsigned) => {
                Some((*id, *generation, unsigned.clone()))
            }
            _ => None,
        }) {
            let signed = unsigned.sign_with_keys(&author).unwrap();
            core.handle(EngineMsg::SignerCompleted(id, generation, Ok(signed)));
        }
    }

    let rows = stalled(&core);
    assert_eq!(rows.len(), 2, "two accepted obligations: {rows:?}");
    assert_ne!(
        rows[0].id, rows[1].id,
        "identical bytes are not the same obligation"
    );
}

/// The projection is rebuilt from durable facts, not from a process-local
/// tally: a crash and reopen reproduces the same descriptor AND the same
/// acceptance instant, which is what makes "stalled since before the
/// restart" answerable at all.
#[test]
fn a_reopen_reproduces_the_same_descriptor_and_acceptance_instant() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stalled.redb");
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://non-existent.example").unwrap();

    let before = {
        let mut core = EngineCore::new(
            RedbStore::open(&path).unwrap(),
            Box::new(directory_knowing(&author, &relay)),
            10,
        );
        activate(&mut core, &author);
        // A real instant, so the reopen has something to reproduce other
        // than zero.
        core.handle(EngineMsg::Tick(Timestamp::now()));
        publish_signed(
            &mut core,
            &author,
            draft(11, "across a process boundary"),
            WriteRouting::Explicit(vec![relay.clone()]),
        );
        let rows = stalled(&core);
        assert_eq!(rows.len(), 1, "expected one stalled write: {rows:?}");
        rows
    };

    let mut reopened = EngineCore::new(
        RedbStore::open(&path).unwrap(),
        Box::new(directory_knowing(&author, &relay)),
        10,
    );
    activate(&mut reopened, &author);
    reopened.recover_on_boot();

    let after = stalled(&reopened);
    assert_eq!(after.len(), 1, "the obligation survived: {after:?}");
    assert_eq!(
        after[0].id, before[0].id,
        "the descriptor is derived from two durable facts, so it cannot move"
    );
    assert_eq!(
        after[0].stalled_since, before[0].stalled_since,
        "an app that restarted and saw a fresh stopwatch would conclude the write had just \
         been accepted"
    );
    assert_eq!(after[0].stage, StalledWriteStage::Undeliverable);
}

/// Diagnostics is a mirror. Reading it — a hundred times — moves no
/// scheduler state: same wake deadline, same worker demand, same answer.
#[test]
fn reading_the_list_changes_no_scheduler_state() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://non-existent.example").unwrap();
    let mut core = EngineCore::new(
        MemoryStore::new(),
        Box::new(directory_knowing(&author, &relay)),
        10,
    );
    activate(&mut core, &author);
    publish_signed(
        &mut core,
        &author,
        draft(21, "read me a hundred times"),
        WriteRouting::Explicit(vec![relay.clone()]),
    );

    let deadline_before = core.next_deadline();
    let first = core.diagnostics_snapshot();
    assert!(
        !first.stalled_writes.is_empty(),
        "there is something to read"
    );

    for _ in 0..100 {
        let again = core.diagnostics_snapshot();
        assert_eq!(
            again.stalled_writes, first.stalled_writes,
            "every read of a mirror shows the same thing"
        );
        assert_eq!(again.stalled_write_totals, first.stalled_write_totals);
    }

    assert_eq!(
        core.next_deadline(),
        deadline_before,
        "reading must not move a wake deadline"
    );
}
