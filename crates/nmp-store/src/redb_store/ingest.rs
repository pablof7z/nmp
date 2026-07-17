use super::ingest_txn::GovernedIngestTxn;
use super::mutation::{
    fan_out_signed_in_txn, process_kind5_deletions, remove_row_in_txn, tombstone_refuses,
};
use super::query::expiration_key;
use super::{
    address_key_for, candidate_wins, BTreeMap, BTreeSet, Event, InsertOutcome, IntentId, Kind,
    PersistenceError, Provenance, RefuseReason, RelayObserved, SigState,
};
#[allow(clippy::too_many_lines)]
pub(super) fn insert_with_tables<T: GovernedIngestTxn>(
    tables: &mut T,
    event: Event,
    from: RelayObserved,
) -> Result<InsertOutcome, PersistenceError> {
    // Refused at the door FIRST: an already-expired event is never
    // stored, so it never touches dedup or supersession at all.
    if event.is_expired_at(&from.at) {
        return Ok(InsertOutcome::Refused(RefuseReason::AlreadyExpired));
    }

    let outcome = {
        if let Some(event_key) = tables.key_for_id(&event.id)? {
            // Dedup-by-id FIRST: merge provenance, no index churn. Goes
            // through `Provenance::merge_observation` (not a re-derived
            // copy) so the persisted backend can never diverge from
            // `MemoryStore`'s merge semantics.
            let mut local = tables.load_local(event_key)?;
            let grew = tables.merge_observation(event_key, &from.relay, from.at)?;
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
                tables.replace_event(event_key, &event)?;
                tables.replace_local(event_key, local)?;
            }
            let satisfied_intents = if let Some(owners) = &fan_out_owners {
                fan_out_signed_in_txn(tables, owners, &event)?
            } else {
                Vec::new()
            };
            InsertOutcome::Duplicate {
                provenance_grew: grew,
                satisfied_intents,
            }
        } else if tombstone_refuses(tables, &event)? {
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
                    let event_key = tables.insert_new(&event, &provenance)?;
                    tables.insert_indexes(&event, event_key)?;
                    if let Some(ts) = event.tags.expiration().copied() {
                        let exp_key = expiration_key(ts, &event.id);
                        tables.expiration_put(&exp_key, event_key)?;
                    }
                    InsertOutcome::Inserted
                }
                Some(addr_key) => {
                    let addr_key_str = addr_key.to_redb_key();
                    let current_key = tables.address_get(addr_key_str.as_str())?;

                    match current_key {
                        None => {
                            let event_key = tables.insert_new(&event, &provenance)?;
                            tables.address_put(addr_key_str.as_str(), event_key)?;
                            tables.insert_indexes(&event, event_key)?;
                            if let Some(ts) = event.tags.expiration().copied() {
                                let exp_key = expiration_key(ts, &event.id);
                                tables.expiration_put(&exp_key, event_key)?;
                            }
                            InsertOutcome::Inserted
                        }
                        Some(current_key) => {
                            let replaced = tables
                                .load_by_key(current_key)?
                                .expect("addr_index must always point at a stored event");
                            let current_event = &replaced.event;

                            if candidate_wins(&event, current_event) {
                                remove_row_in_txn(tables, current_event.id, |_| true)?
                                    .expect("addr_index must always point at a stored event");
                                let event_key = tables.insert_new(&event, &provenance)?;
                                tables.address_put(addr_key_str.as_str(), event_key)?;
                                tables.insert_indexes(&event, event_key)?;
                                if let Some(ts) = event.tags.expiration().copied() {
                                    let exp_key = expiration_key(ts, &event.id);
                                    tables.expiration_put(&exp_key, event_key)?;
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
                    let deleted = process_kind5_deletions(tables, &event)?;
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
