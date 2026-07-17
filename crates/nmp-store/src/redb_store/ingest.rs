use super::canonical::CanonicalWriteTables;
use super::mutation::{
    fan_out_signed_in_txn, process_kind5_deletions, remove_row_in_txn, tombstone_refuses,
};
use super::outbox::{OUTBOX_KIND5_CLAIMS, OUTBOX_SUPPRESS_BY_ADDR, OUTBOX_SUPPRESS_BY_ID};
use super::query::{expiration_key, insert_query_index_rows, QueryIndexWriteTables};
use super::schema::{
    persist_err, EventKey, ADDR_INDEX, ADDR_TOMBSTONES, EXPIRATION_INDEX, OUTBOX_DISPLACED,
    OUTBOX_INTENTS, OUTBOX_RECEIPTS, TOMBSTONES,
};
use super::{
    address_key_for, candidate_wins, BTreeMap, BTreeSet, Event, InsertOutcome, IntentId, Kind,
    PersistenceError, Provenance, RefuseReason, RelayObserved, SigState,
};
use redb::ReadableTable;

pub(super) struct InsertWriteTables<'txn> {
    pub(super) canonical: CanonicalWriteTables<'txn>,
    pub(super) addr_index: redb::Table<'txn, &'static str, EventKey>,
    pub(super) tombstones: redb::Table<'txn, &'static str, &'static str>,
    pub(super) addr_tombstones: redb::Table<'txn, &'static str, &'static str>,
    pub(super) expiration_index: redb::Table<'txn, &'static [u8; 40], EventKey>,
    pub(super) indexes: QueryIndexWriteTables<'txn>,
    pub(super) outbox_intents: redb::Table<'txn, &'static str, &'static str>,
    pub(super) outbox_receipts: redb::Table<'txn, &'static str, &'static str>,
    pub(super) outbox_displaced: redb::Table<'txn, &'static str, &'static [u8]>,
    pub(super) outbox_kind5_claims: redb::Table<'txn, &'static str, &'static str>,
    pub(super) outbox_suppress_by_id: redb::Table<'txn, &'static str, &'static str>,
    pub(super) outbox_suppress_by_addr: redb::Table<'txn, &'static str, &'static str>,
}

impl<'txn> InsertWriteTables<'txn> {
    pub(super) fn open(write_txn: &'txn redb::WriteTransaction) -> Result<Self, PersistenceError> {
        Ok(Self {
            canonical: CanonicalWriteTables::open(write_txn)?,
            addr_index: write_txn.open_table(ADDR_INDEX).map_err(persist_err)?,
            tombstones: write_txn.open_table(TOMBSTONES).map_err(persist_err)?,
            addr_tombstones: write_txn.open_table(ADDR_TOMBSTONES).map_err(persist_err)?,
            expiration_index: write_txn
                .open_table(EXPIRATION_INDEX)
                .map_err(persist_err)?,
            indexes: QueryIndexWriteTables::open(write_txn)?,
            outbox_intents: write_txn.open_table(OUTBOX_INTENTS).map_err(persist_err)?,
            outbox_receipts: write_txn.open_table(OUTBOX_RECEIPTS).map_err(persist_err)?,
            outbox_displaced: write_txn
                .open_table(OUTBOX_DISPLACED)
                .map_err(persist_err)?,
            outbox_kind5_claims: write_txn
                .open_table(OUTBOX_KIND5_CLAIMS)
                .map_err(persist_err)?,
            outbox_suppress_by_id: write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ID)
                .map_err(persist_err)?,
            outbox_suppress_by_addr: write_txn
                .open_table(OUTBOX_SUPPRESS_BY_ADDR)
                .map_err(persist_err)?,
        })
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn insert_with_tables(
    tables: &mut InsertWriteTables<'_>,
    event: Event,
    from: RelayObserved,
) -> Result<InsertOutcome, PersistenceError> {
    // Refused at the door FIRST: an already-expired event is never
    // stored, so it never touches dedup or supersession at all.
    if event.is_expired_at(&from.at) {
        return Ok(InsertOutcome::Refused(RefuseReason::AlreadyExpired));
    }

    let InsertWriteTables {
        canonical,
        addr_index,
        tombstones,
        addr_tombstones,
        expiration_index,
        indexes,
        outbox_intents,
        outbox_receipts,
        outbox_displaced,
        outbox_kind5_claims,
        outbox_suppress_by_id,
        outbox_suppress_by_addr,
    } = tables;
    let outcome = {
        if let Some(event_key) = canonical.key_for_id(&event.id)? {
            // Dedup-by-id FIRST: merge provenance, no index churn. Goes
            // through `Provenance::merge_observation` (not a re-derived
            // copy) so the persisted backend can never diverge from
            // `MemoryStore`'s merge semantics.
            let mut local = canonical.load_local(event_key)?;
            let grew = canonical.merge_observation(event_key, &from.relay, from.at)?;
            // Architecture review requirement (issue #2 P0 correction,
            // codex-nova ruling): a relay delivering the real signed
            // event for a still-Pending local draft is functionally the
            // SAME signature-adoption/fan-out invariant `promote_signed`
            // performs explicitly — adopt it, mark every co-owner
            // `Signed`, and fan out, rather than silently keeping our
            // own sentinel forever (`event` here is, by this door's own
            // contract, always a genuine relay delivery, never our OWN
            // sentinel, so its signature is always safe to adopt).
            let needs_adoption = local
                .as_ref()
                .is_some_and(|l| l.sig_state == SigState::Pending);
            let mut fan_out_owners: Option<BTreeSet<IntentId>> = None;
            if needs_adoption {
                let mut adopted = local
                    .clone()
                    .expect("just checked this row carries local provenance");
                adopted.sig_state = SigState::Signed;
                fan_out_owners = Some(adopted.owners.clone());
                local = Some(adopted);
            }
            // `merge_observation` never touches `local` (a relay echo
            // of an already-local row keeps its local provenance,
            // retraction doc §4.1) — `provenance.local` is otherwise
            // unchanged, written straight back.
            if fan_out_owners.is_some() {
                canonical.replace_event(event_key, &event)?;
                canonical.replace_local(event_key, local)?;
            }
            let satisfied_intents = if let Some(owners) = &fan_out_owners {
                fan_out_signed_in_txn(
                    canonical,
                    addr_index,
                    tombstones,
                    addr_tombstones,
                    expiration_index,
                    indexes,
                    outbox_intents,
                    outbox_receipts,
                    outbox_displaced,
                    outbox_kind5_claims,
                    outbox_suppress_by_id,
                    outbox_suppress_by_addr,
                    owners,
                    &event,
                )?
            } else {
                Vec::new()
            };
            InsertOutcome::Duplicate {
                provenance_grew: grew,
                satisfied_intents,
            }
        } else if tombstone_refuses(tombstones, addr_tombstones, &event)? {
            // Tombstone check, AFTER dedup-by-id, BEFORE storage
            // (retraction-and-negative-deltas.md §2).
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        } else {
            let is_deletion = event.kind == Kind::EventDeletion;
            let provenance = Provenance {
                seen: BTreeMap::from([(from.relay.clone(), from.at)]),
                local: None,
            };

            let outcome = match address_key_for(&event) {
                None => {
                    let event_key = canonical.insert_new(&event, &provenance)?;
                    insert_query_index_rows(canonical, indexes, &event, event_key)
                        .map_err(persist_err)?;
                    if let Some(ts) = event.tags.expiration().copied() {
                        let exp_key = expiration_key(ts, &event.id);
                        expiration_index
                            .insert(&exp_key, event_key)
                            .map_err(persist_err)?;
                    }
                    InsertOutcome::Inserted
                }
                Some(addr_key) => {
                    let addr_key_str = addr_key.to_redb_key();
                    let current_key = addr_index
                        .get(addr_key_str.as_str())
                        .map_err(persist_err)?
                        .map(|guard| guard.value());

                    match current_key {
                        None => {
                            let event_key = canonical.insert_new(&event, &provenance)?;
                            addr_index
                                .insert(addr_key_str.as_str(), event_key)
                                .map_err(persist_err)?;
                            insert_query_index_rows(canonical, indexes, &event, event_key)
                                .map_err(persist_err)?;
                            if let Some(ts) = event.tags.expiration().copied() {
                                let exp_key = expiration_key(ts, &event.id);
                                expiration_index
                                    .insert(&exp_key, event_key)
                                    .map_err(persist_err)?;
                            }
                            InsertOutcome::Inserted
                        }
                        Some(current_key) => {
                            let replaced = canonical
                                .load_by_key(current_key)?
                                .expect("addr_index must always point at a stored event");
                            let current_event = &replaced.event;

                            if candidate_wins(&event, current_event) {
                                remove_row_in_txn(
                                    canonical,
                                    addr_index,
                                    expiration_index,
                                    indexes,
                                    current_event.id,
                                    |_| true,
                                )?
                                .expect("addr_index must always point at a stored event");
                                let event_key = canonical.insert_new(&event, &provenance)?;
                                addr_index
                                    .insert(addr_key_str.as_str(), event_key)
                                    .map_err(persist_err)?;
                                insert_query_index_rows(canonical, indexes, &event, event_key)
                                    .map_err(persist_err)?;
                                if let Some(ts) = event.tags.expiration().copied() {
                                    let exp_key = expiration_key(ts, &event.id);
                                    expiration_index
                                        .insert(&exp_key, event_key)
                                        .map_err(persist_err)?;
                                }
                                InsertOutcome::Superseded {
                                    replaced: Box::new(replaced),
                                }
                            } else {
                                InsertOutcome::Stale
                            }
                        }
                    }
                }
            };

            // kind:5 has no replaceable/addressable address (M1's set
            // excludes it), so `outcome` above is always `Inserted`
            // here, by construction -- process its deletions now that
            // the event itself is durably stored (re-servable, §2).
            if is_deletion {
                if let InsertOutcome::Inserted = outcome {
                    let deleted = process_kind5_deletions(
                        canonical,
                        addr_index,
                        tombstones,
                        addr_tombstones,
                        expiration_index,
                        indexes,
                        &event,
                    )?;
                    InsertOutcome::Kind5Processed { deleted }
                } else {
                    outcome
                }
            } else {
                outcome
            }
        }
    };
    Ok(outcome)
}
