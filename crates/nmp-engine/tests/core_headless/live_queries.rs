use super::*;

// ---- live query delivery and evidence ----------------------------------

#[test]
fn subscribe_opens_wire_for_resolved_demand() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));

    let (_sub_id, filter) = req_for(&effects, &relay0);
    assert_eq!(filter, &cf(&[1], &[&a.public_key().to_hex()]));
}

#[test]
fn ingest_frame_recompiles_wire_and_emits_rows() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(b.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);

    connect(&mut core, 0, &relay0);

    // $myFollows shape: kinds:[1], authors := Derived(inner=kind:3 by me,
    // project=#p) -- exactly nmp-resolver's M1 contract-test shape.
    let my_follows = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(nmp_grammar::Derived {
            inner: nmp_grammar::Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
                ..Filter::default()
            }),
            project: nmp_grammar::Selector::Tag("p".to_string()),
        }))),
        ..Filter::default()
    });

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::SetActivePubkey(Some(a.public_key())));
    let _ = core.handle(EngineMsg::Subscribe(my_follows, Box::new(sink.clone())));

    // B's kind:1 post arrives UNSOLICITED (before B is ever followed) --
    // the store holds it, but it matches no handle's root atoms yet.
    let b_post = nmp_resolver::testkit::kind1(&b, "hello from b", 50);
    let pre_effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        event_frame("s", b_post.clone()),
    ));
    assert!(
        !pre_effects
            .iter()
            .any(|e| matches!(e, Effect::EmitRows(_, rows, _) if !rows.is_empty())),
        "b's post must not be visible before b is followed"
    );

    // Now `a` follows `b`: root atoms fan out to include {kind:1,
    // authors:{b}} -- demand changes (Wire opens b's write relay) AND the
    // handle's row set changes (b's pre-existing post is now in scope).
    let contact_list = nmp_resolver::testkit::kind3(&a, &[b.public_key()], 100);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        event_frame("s", contact_list),
    ));

    assert!(
        effects.iter().any(|e| matches!(e, Effect::Wire(_))),
        "ingest must recompile and open the new author's atom on the wire"
    );
    let emitted = effects.iter().find_map(|e| match e {
        Effect::EmitRows(_, rows, _) => Some(rows),
        _ => None,
    });
    let rows = emitted.expect("ingest must emit rows for the affected handle");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].event().map(|e| e.id),
        Some(b_post.id),
        "the single delta must be an Added(b_post), never a Removed or a re-delivered full set"
    );

    // The sink was also called synchronously with the same rows.
    let captured = sink.0.lock().unwrap();
    assert!(captured
        .iter()
        .any(|batch| batch.len() == 1 && batch[0].event().map(|e| e.id) == Some(b_post.id)));
}

// ---- P0 load test (docs/known-gaps.md): redelivery must be O(distinct
// rows), never O(rows^2) --------------------------------------------------

/// The falsifier for the P0 dogfooding bug: before the `RowDelta::Added`/
/// `Removed` delta fix, `EngineCore::refresh_handle` re-emitted the FULL
/// current row set on every single ingested event (because
/// `rows_and_coverage_for` always recomputed -- and `EmitRows` always
/// carried -- every currently-matching row, not just what changed). N
/// distinct matching events therefore delivered ~N*(N+1)/2 total rows
/// across the run -- O(N^2) -- confirmed live against real relays as a
/// 635-1294x redelivery ratio (~3.35M raw row deliveries for ~2,587
/// distinct notes in 20s). This test subscribes once, then ingests N=2,000
/// distinct matching events ONE AT A TIME through the real
/// `EngineMsg::RelayFrame` ingest path (exactly what a live relay stream
/// does -- `on_relay_frame`'s `Event` arm always calls `recompile` +
/// `refresh_all_handles`), and asserts the TOTAL number of row-delta
/// entries delivered across every `EmitRows` batch stays close to N (each
/// distinct row delivered ~once), nowhere near the O(N^2) blow-up the old
/// full-set-re-emit behavior produced. Bounded/deterministic: a fixed N,
/// no network, and a generous wall-clock ceiling so an O(N^2) regression
/// fails loudly instead of hanging.
#[test]
fn ingesting_n_distinct_events_delivers_order_n_row_entries_not_order_n_squared() {
    let start = Instant::now();
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(sink.clone()),
    ));

    const N: u64 = 2_000;
    let mut total_delta_entries = 0usize;
    for i in 0..N {
        let event = nmp_resolver::testkit::kind1(&a, &format!("load-test post #{i}"), 1_000 + i);
        let effects = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            public_session(&relay0),
            event_frame("s", event),
        ));
        for effect in &effects {
            if let Effect::EmitRows(_, rows, _) = effect {
                total_delta_entries += rows.len();
            }
        }
    }

    // The fix must not have traded over-delivery for under-delivery: every
    // one of the N distinct events actually reaches the sink at least once
    // (as an `Added`), or this "load test" would be vacuous.
    let captured = sink.0.lock().unwrap();
    let distinct_delivered: BTreeSet<nostr::EventId> = captured
        .iter()
        .flatten()
        .filter_map(RowDelta::event)
        .map(|e| e.id)
        .collect();
    assert_eq!(
        distinct_delivered.len(),
        N as usize,
        "every one of the N distinct ingested events must be delivered at least once"
    );

    // THE falsifier: total delivered row-delta entries stays ~O(N) (a small
    // constant multiple covers the initial empty-subscribe batch and any
    // coverage-only re-emits), nowhere near the O(N^2) blow-up a full-set
    // re-emit would produce (~N*(N+1)/2 = 2,001,000 for N=2,000 -- 500x+
    // this bound).
    let quadratic_blowup = (N * (N + 1)) / 2;
    assert!(
        total_delta_entries < (N as usize) * 2,
        "total delivered row-delta entries ({total_delta_entries}) must stay ~O(N) -- the \
         old full-set-re-emit bug would have delivered ~{quadratic_blowup} (O(N^2))"
    );

    assert!(
        start.elapsed() < Duration::from_secs(30),
        "load test must complete quickly -- an O(N^2) regression would blow this budget \
         (elapsed: {:?})",
        start.elapsed()
    );
}

// ---- #124: a demand's NIP-01 `limit:N` projects only the N newest rows ---

/// A literal-author query carrying an explicit NIP-01 `limit:N`.
fn limited_literal_query(kinds: &[u16], author_hex: &str, limit: usize) -> LiveQuery {
    LiveQuery::from_filter(Filter {
        kinds: Some(kinds.iter().copied().collect()),
        authors: Some(Binding::Literal(BTreeSet::from([author_hex.to_string()]))),
        limit: Some(limit),
        ..Filter::default()
    })
}

/// Fold one delivered `RowDelta` batch into a running "current row set" of
/// event ids, exactly as an app consuming the reactive stream would.
fn apply_deltas(current: &mut BTreeSet<nostr::EventId>, batch: &[RowDelta]) {
    for delta in batch {
        match delta {
            RowDelta::Added(row) => {
                current.insert(row.event.id);
            }
            RowDelta::Removed(id) => {
                current.remove(id);
            }
            RowDelta::SourcesGrew { .. } => {}
        }
    }
}

/// (a) With M > N matching cached events, the handle projects EXACTLY the N
/// newest by `created_at` DESC (id ASC tie-break) -- never every cached
/// match. Feeds five kind:1 events (created_at 10..50) one at a time into a
/// `limit:3` handle and asserts the folded current set is precisely the three
/// newest, and that it never grew past N at any point along the way.
#[test]
fn limited_handle_projects_only_the_n_newest_of_m_matches() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::Subscribe(
        limited_literal_query(&[1], &a.public_key().to_hex(), 3),
        Box::new(sink.clone()),
    ));

    let mut ids_by_time: Vec<(u64, nostr::EventId)> = Vec::new();
    for created_at in [10u64, 20, 30, 40, 50] {
        let event = nmp_resolver::testkit::kind1(&a, &format!("note @{created_at}"), created_at);
        ids_by_time.push((created_at, event.id));
        let _ = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            public_session(&relay0),
            event_frame("s", event),
        ));
    }

    // Replay the delivered stream; assert it never exceeds N mid-flight.
    let mut current = BTreeSet::new();
    let mut high_water = 0usize;
    for batch in sink.0.lock().unwrap().iter() {
        apply_deltas(&mut current, batch);
        high_water = high_water.max(current.len());
    }
    assert!(
        high_water <= 3,
        "a limit:3 handle must never accumulate more than 3 rows (peak was {high_water})"
    );

    let expected: BTreeSet<nostr::EventId> = ids_by_time
        .iter()
        .rev()
        .take(3)
        .map(|(_, id)| *id)
        .collect();
    assert_eq!(
        current, expected,
        "the projected set must be exactly the 3 newest (created_at 30/40/50), not all 5"
    );
}

/// Pre-bounding each fanned root atom to N remains exact only if the engine
/// still applies the authoritative N cap after merging the atoms. Two
/// authors fan into two root atoms here; the global top-2 must contain one
/// event from each author, not either atom's local top-2 wholesale.
#[test]
fn limited_multi_atom_handle_merges_then_applies_the_global_top_n() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(b.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::Subscribe(
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Literal(BTreeSet::from([
                a.public_key().to_hex(),
                b.public_key().to_hex(),
            ]))),
            limit: Some(2),
            ..Filter::default()
        }),
        Box::new(sink.clone()),
    ));

    let a_100 = nmp_resolver::testkit::kind1(&a, "a-100", 100);
    let a_90 = nmp_resolver::testkit::kind1(&a, "a-90", 90);
    let b_95 = nmp_resolver::testkit::kind1(&b, "b-95", 95);
    let b_85 = nmp_resolver::testkit::kind1(&b, "b-85", 85);
    for event in [a_90, b_85, a_100.clone(), b_95.clone()] {
        let _ = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            public_session(&relay0),
            event_frame("s", event),
        ));
    }

    let mut current = BTreeSet::new();
    for batch in sink.0.lock().unwrap().iter() {
        apply_deltas(&mut current, batch);
    }
    assert_eq!(
        current,
        BTreeSet::from([a_100.id, b_95.id]),
        "the final per-subscription cap must select the global top-2 after merging both atoms"
    );
}

/// (b) A newer matching event entering the top-N evicts the oldest of the N:
/// the ingest emits Added(new) + Removed(oldest) and the set stays at N,
/// proving the reactive DELTA path (not just a fresh snapshot) maintains the
/// window.
#[test]
fn newer_event_evicts_oldest_of_top_n_via_delta() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::Subscribe(
        limited_literal_query(&[1], &a.public_key().to_hex(), 2),
        Box::new(sink.clone()),
    ));

    let oldest = nmp_resolver::testkit::kind1(&a, "oldest", 100);
    let middle = nmp_resolver::testkit::kind1(&a, "middle", 200);
    for event in [oldest.clone(), middle.clone()] {
        let _ = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            public_session(&relay0),
            event_frame("s", event),
        ));
    }

    // The top-2 is now {oldest, middle}. A strictly newer event arrives.
    let newest = nmp_resolver::testkit::kind1(&a, "newest", 300);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        event_frame("s", newest.clone()),
    ));
    let batch = effects
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(_, rows, _) => Some(rows.clone()),
            _ => None,
        })
        .expect("the newer event must emit a row delta");

    assert!(
        batch
            .iter()
            .any(|d| matches!(d, RowDelta::Added(row) if row.event.id == newest.id)),
        "the newer event must be Added: {batch:?}"
    );
    assert!(
        batch
            .iter()
            .any(|d| matches!(d, RowDelta::Removed(id) if *id == oldest.id)),
        "the evicted oldest of the top-N must be Removed: {batch:?}"
    );
    assert!(
        !batch.iter().any(|d| d.id() == middle.id),
        "the surviving middle row must not churn (no delta for it): {batch:?}"
    );

    let mut current = BTreeSet::new();
    for b in sink.0.lock().unwrap().iter() {
        apply_deltas(&mut current, b);
    }
    assert_eq!(
        current,
        BTreeSet::from([middle.id, newest.id]),
        "the window must hold exactly the 2 newest after the churn"
    );
}

/// (c) Retracting a member of the current top-N pulls the next-newest
/// (previously excluded) match IN: the retraction emits Removed(retracted) +
/// Added(next-newest), and the set stays at N.
#[test]
fn retracting_top_n_member_pulls_in_next_newest() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let sink = CapturingSink::default();
    let _ = core.handle(EngineMsg::Subscribe(
        limited_literal_query(&[1], &a.public_key().to_hex(), 2),
        Box::new(sink.clone()),
    ));

    // Three matches; the top-2 is {second, third}, `first` is excluded.
    let first = nmp_resolver::testkit::kind1(&a, "first", 100);
    let second = nmp_resolver::testkit::kind1(&a, "second", 200);
    let third = nmp_resolver::testkit::kind1(&a, "third", 300);
    for event in [first.clone(), second.clone(), third.clone()] {
        let _ = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            public_session(&relay0),
            event_frame("s", event),
        ));
    }
    {
        let mut current = BTreeSet::new();
        for b in sink.0.lock().unwrap().iter() {
            apply_deltas(&mut current, b);
        }
        assert_eq!(
            current,
            BTreeSet::from([second.id, third.id]),
            "precondition: the window holds the 2 newest, excluding `first`"
        );
    }

    // Retract `third` (a current top-N member) via a NIP-09 kind:5 delete.
    let deletion = nmp_resolver::testkit::deletion(&a, &[third.id], 400);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        event_frame("s", deletion),
    ));
    let batch = effects
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(_, rows, _) => Some(rows.clone()),
            _ => None,
        })
        .expect("retracting a held row must emit a row delta");
    assert!(
        batch
            .iter()
            .any(|d| matches!(d, RowDelta::Removed(id) if *id == third.id)),
        "the retracted top-N member must be Removed: {batch:?}"
    );
    assert!(
        batch
            .iter()
            .any(|d| matches!(d, RowDelta::Added(row) if row.event.id == first.id)),
        "the next-newest previously-excluded match must be pulled IN as Added: {batch:?}"
    );

    let mut current = BTreeSet::new();
    for b in sink.0.lock().unwrap().iter() {
        apply_deltas(&mut current, b);
    }
    assert_eq!(
        current,
        BTreeSet::from([first.id, second.id]),
        "after retraction the window refills to the next 2 newest"
    );
}

/// (d) `limit: None` is unchanged -- every matching row is projected, with no
/// truncation.
#[test]
fn unlimited_handle_projects_every_match() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let sink = CapturingSink::default();
    // `literal_query` carries no limit (limit: None).
    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(sink.clone()),
    ));

    let mut all_ids = BTreeSet::new();
    for created_at in [10u64, 20, 30, 40, 50] {
        let event = nmp_resolver::testkit::kind1(&a, &format!("note @{created_at}"), created_at);
        all_ids.insert(event.id);
        let _ = core.handle(EngineMsg::RelayFrame(
            RelayHandle {
                slot: 0,
                generation: 1,
            },
            public_session(&relay0),
            event_frame("s", event),
        ));
    }

    let mut current = BTreeSet::new();
    for b in sink.0.lock().unwrap().iter() {
        apply_deltas(&mut current, b);
    }
    assert_eq!(
        current, all_ids,
        "with no limit, every one of the 5 matching rows must be projected"
    );
}

// ---- test 2 analog: EOSE records a watermark; a bare EVENT never does ---

#[test]
fn eose_records_coverage_watermark_and_non_eose_does_not() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let atom = cf(&[3], &[&a.public_key().to_hex()]);
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[3], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (sub_id, _filter) = req_for(&effects, &relay0);
    let wire = wire_sub_string(sub_id);

    // A bare EVENT frame (no EOSE yet) must record nothing.
    let e = nmp_resolver::testkit::kind3(&a, &[], 10);
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        event_frame(&wire, e),
    ));
    assert_eq!(
        core.get_coverage(&ctx_atom(atom.clone()), &relay0),
        None,
        "presence != coverage"
    );

    // The EOSE proves the (unfloored) window up to the engine clock.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire),
    ));

    let interval = core
        .get_coverage(&ctx_atom(atom.clone()), &relay0)
        .expect("EOSE must record a coverage row");
    assert_eq!(interval.from, Timestamp::from(0u64));
    assert_eq!(interval.through, Timestamp::from(500u64));
}

/// #118's headline falsifier (fixed ahead of #107): a `Demand` explicitly
/// declared `Public` over an author-bearing selection (#106's "new
/// expressible behavior" -- "these authors, generic facts only, no outbox
/// chase") is a genuinely DIFFERENT coverage identity than the SAME
/// selection under the static-default `AuthorOutboxes` guess. Proves
/// `get_coverage` now reads the atom's TRUE declared context: querying
/// under the correct (`Public`) context finds the recorded coverage;
/// querying under the static default's WRONG guess (`AuthorOutboxes`,
/// since the filter IS author-bearing) does not -- exactly the silent
/// re-alias #118 describes, now provably closed.
#[test]
fn get_coverage_distinguishes_true_context_from_the_static_default_guess() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let filter = cf(&[1], &[&a.public_key().to_hex()]);
    // A directory fact so the Public-sourced atom (classify() sends
    // `Public` straight to the pinned/directory lookup, never the outbox
    // solver) actually routes somewhere.
    let dir = FixtureDirectory::new().with_group_host(filter.clone(), relay0.clone());
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let demand = nmp_grammar::Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Literal(BTreeSet::from([a.public_key().to_hex()]))),
            ..Filter::default()
        },
        SourceAuthority::Public,
        AccessContext::Public,
    )
    .expect("Public over an author-bearing selection is legal (#106)");

    let effects = core.handle(EngineMsg::Subscribe(
        LiveQuery(demand),
        Box::new(CapturingSink::default()),
    ));
    let (sub_id, _f) = req_for(&effects, &relay0);
    let wire = wire_sub_string(sub_id);

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire),
    ));

    assert!(
        core.get_coverage(
            &ctx_atom_with(filter.clone(), SourceAuthority::Public),
            &relay0
        )
        .is_some(),
        "the TRUE declared context (Public) must find the recorded coverage"
    );
    assert!(
        core.get_coverage(&ctx_atom(filter), &relay0).is_none(),
        "the static-default's WRONG guess (AuthorOutboxes, since the filter is \
         author-bearing) must NOT find coverage recorded under a genuinely \
         different declared context"
    );
}

/// #107's core Done-when trio, exercised as one flow since they compose
/// naturally: (1) Agnostic pinned-R1 returns a matching cached R2-only row
/// while wire contacts only R1; (2) Strict pinned-R1 excludes that same row
/// until it is observed from R1 too; (6) same-filter Agnostic and Strict
/// handles remain distinct even though they share ONE wire subscription
/// (`AcquisitionKey` excludes `cache`, #106/#107's ratified shape -- two
/// handles differing ONLY in `cache` dedup onto the identical graph node/
/// wire/coverage, per `nmp-resolver::Engine::subscribe`'s own doc).
#[test]
fn agnostic_and_strict_pinned_handles_project_distinct_rows_from_one_shared_wire() {
    let a = Keys::generate();
    let relay_other = RelayUrl::parse("wss://other.example.com").unwrap();
    let relay_pinned = RelayUrl::parse("wss://pinned.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay_other.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay_other);
    connect(&mut core, 1, &relay_pinned);

    // Seed the store: an ordinary AuthorOutboxes subscribe pulls the event
    // in from relay_other, giving it Row.sources == {relay_other}.
    let outbox_effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (outbox_sub, _f) = req_for(&outbox_effects, &relay_other);
    let outbox_wire = wire_sub_string(outbox_sub);
    let event = unsigned(&a, 1, "seeded via relay_other")
        .sign_with_keys(&a)
        .expect("sign fixture event");
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay_other),
        event_frame(&outbox_wire, event.clone()),
    ));

    // Two NEW handles over the IDENTICAL selection, both declared
    // SourceAuthority::Pinned({relay_pinned}) -- the SAME AcquisitionKey --
    // but one Agnostic (the default), one Strict.
    let filter = Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([a.public_key().to_hex()]))),
        ..Filter::default()
    };
    let pinned_relays = BTreeSet::from([relay_pinned.clone()]);
    let agnostic_demand = nmp_grammar::Demand::new(
        filter,
        SourceAuthority::Pinned(pinned_relays),
        AccessContext::Public,
    )
    .expect("a nonempty pinned relay set is legal (#107)");
    let mut strict_demand = agnostic_demand.clone();
    strict_demand.cache = nmp_grammar::CacheMode::Strict;

    let effects_agnostic = core.handle(EngineMsg::Subscribe(
        LiveQuery(agnostic_demand),
        Box::new(CapturingSink::default()),
    ));

    // Wire contacts ONLY the declared pinned relay for this new atom --
    // never relay_other (no re-req there at all: nothing about that atom
    // changed), and (since this fixture directory configures no app/
    // fallback/indexer/group-host facts) there is nowhere else it even
    // COULD leak to.
    let (pinned_sub, _f) = req_for(&effects_agnostic, &relay_pinned);
    let pinned_wire = wire_sub_string(pinned_sub);
    assert!(
        !effects_agnostic.iter().any(|effect| matches!(
            effect,
            Effect::Wire(delta) if delta.ops.iter().any(|(r, _)| r.relay == relay_other)
        )),
        "an ExplicitPinned atom's subscribe must never recompile a Req/Close at any \
         relay but its own declared set"
    );
    assert!(
        all_row_deltas(&effects_agnostic)
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == event.id)),
        "Agnostic must return a matching cached row regardless of its recorded provenance"
    );

    // The Strict handle dedups onto the SAME graph/wire (no new Req at
    // relay_pinned), yet must NOT see the row: its provenance ({relay_other})
    // is disjoint from the pinned set ({relay_pinned}).
    let effects_strict = core.handle(EngineMsg::Subscribe(
        LiveQuery(strict_demand),
        Box::new(CapturingSink::default()),
    ));
    assert!(
        !effects_strict
            .iter()
            .any(|effect| matches!(effect, Effect::Wire(_))),
        "a Strict handle sharing the identical AcquisitionKey must dedup onto the \
         existing wire subscription, never open a second one"
    );
    assert!(
        !all_row_deltas(&effects_strict)
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == event.id)),
        "Strict must exclude a row whose recorded provenance is disjoint from the \
         pinned relay set"
    );

    // The SAME event now arrives from the pinned relay too: the Strict
    // handle must pick it up the instant its own provenance intersects the
    // pinned set, and the Agnostic handle (which already had it) must still
    // record the provenance growth -- both are the SAME underlying
    // `Row.sources` growing, projected differently per handle's `cache`.
    let after = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        public_session(&relay_pinned),
        event_frame(&pinned_wire, event.clone()),
    ));
    let deltas = all_row_deltas(&after);
    assert!(
        deltas.iter().any(|delta| matches!(
            delta,
            RowDelta::Added(row) if row.event.id == event.id && row.sources.contains(&relay_pinned)
        )),
        "the Strict handle must newly Add the row once its provenance includes the \
         pinned relay: {deltas:?}"
    );
    assert!(
        deltas.iter().any(|delta| matches!(
            delta,
            RowDelta::SourcesGrew { id, sources } if *id == event.id && sources.contains(&relay_pinned)
        )),
        "the Agnostic handle's already-visible row must still record the provenance \
         growth: {deltas:?}"
    );
}

/// #107's remaining Done-when trio item: "Equal filters pinned to R1 and R2
/// retain distinct row projections, evidence, EOSE facts, and teardown."
/// Unlike the Agnostic/Strict test above (same pinned set, different cache
/// mode, sharing ONE wire subscription), this is the OTHER axis: the
/// IDENTICAL filter pinned to two DIFFERENT relay sets is a genuinely
/// different `SourceAuthority::Pinned` value, hence a different
/// `AcquisitionKey` -- two fully independent handles, subs, and EOSE
/// watermarks, never sharing so much as a wire request.
#[test]
fn identical_filter_pinned_to_different_relays_stays_fully_independent() {
    let a = Keys::generate();
    let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
    let relay2 = RelayUrl::parse("wss://relay2.example.com").unwrap();
    let mut core = new_core(FixtureDirectory::new());
    connect(&mut core, 0, &relay1);
    connect(&mut core, 1, &relay2);

    let filter = Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([a.public_key().to_hex()]))),
        ..Filter::default()
    };
    let demand1 = nmp_grammar::Demand::new(
        filter.clone(),
        SourceAuthority::Pinned(BTreeSet::from([relay1.clone()])),
        AccessContext::Public,
    )
    .expect("nonempty pinned relay set is legal");
    let demand2 = nmp_grammar::Demand::new(
        filter,
        SourceAuthority::Pinned(BTreeSet::from([relay2.clone()])),
        AccessContext::Public,
    )
    .expect("nonempty pinned relay set is legal");

    let effects1 = core.handle(EngineMsg::Subscribe(
        LiveQuery(demand1),
        Box::new(CapturingSink::default()),
    ));
    let id1 = effects1
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(hid, ..) => Some(*hid),
            _ => None,
        })
        .expect("subscribe must emit an initial EmitRows for its own handle");
    let (sub1, _) = req_for(&effects1, &relay1);
    let wire1 = wire_sub_string(sub1);
    assert!(
        !effects1.iter().any(
            |e| matches!(e, Effect::Wire(delta) if delta.ops.iter().any(|(r, _)| r.relay == relay2))
        ),
        "demand1's Pinned({{relay1}}) atom must never touch relay2"
    );

    let effects2 = core.handle(EngineMsg::Subscribe(
        LiveQuery(demand2),
        Box::new(CapturingSink::default()),
    ));
    let id2 = effects2
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(hid, ..) => Some(*hid),
            _ => None,
        })
        .expect("subscribe must emit an initial EmitRows for its own handle");
    let (sub2, _) = req_for(&effects2, &relay2);
    let _wire2 = wire_sub_string(sub2);
    assert_ne!(
        id1, id2,
        "two distinct subscribe calls must yield distinct handles"
    );
    assert_ne!(
        sub1, sub2,
        "distinct pinned relay sets over an identical filter must never share a SubId"
    );
    assert!(
        !effects2.iter().any(
            |e| matches!(e, Effect::Wire(delta) if delta.ops.iter().any(|(r, _)| r.relay == relay1))
        ),
        "demand2's Pinned({{relay2}}) atom must never touch relay1 -- and must not even \
         re-touch relay1's already-open sub, since these are independent graph nodes"
    );

    // Distinct EOSE facts: only relay1's sub finishes -- handle1's OWN
    // relay1 entry advances; handle2's relay2 entry (a DIFFERENT handle
    // entirely) must stay unproven.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(10u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay1),
        eose_frame(&wire1),
    ));
    let evidence1 = evidence_from(&effects, id1).expect("relay1's EOSE must refresh handle1");
    let r1 = source_for(evidence1, &relay1).expect("relay1 must be a source for handle1");
    assert_eq!(r1.reconciled_through, Some(Timestamp::from(10u64)));
    assert!(
        evidence_from(&effects, id2).is_none()
            || source_for(evidence_from(&effects, id2).unwrap(), &relay2)
                .is_none_or(|r2| r2.reconciled_through.is_none()),
        "handle2's relay2 entry must NOT advance off handle1's relay1 EOSE"
    );

    // Distinct teardown: unsubscribing handle1 closes ONLY relay1's sub;
    // handle2's relay2 subscription is untouched.
    let teardown = core.handle(EngineMsg::Unsubscribe(id1));
    let closed_relays: BTreeSet<RelayUrl> = teardown
        .iter()
        .filter_map(|e| match e {
            Effect::Wire(delta) => Some(
                delta
                    .ops
                    .iter()
                    .map(|(session, _)| session.relay.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        closed_relays,
        BTreeSet::from([relay1]),
        "unsubscribing handle1 must close exactly relay1's sub, never touch relay2's"
    );
}

// ---- the EOSE-overwrite-race rule (ruling §2) ---------------------------

#[test]
fn eose_overwrite_race_credits_only_the_intersection() {
    let a = Keys::generate();
    let e_key = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(e_key.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    // First subscribe: sends REQ(sub, {authors:{a}}) -- snapshot1 absorbs
    // {h_a} only.
    let effects1 = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (sub_id, _f) = req_for(&effects1, &relay0);
    let sub_id = sub_id.clone();
    let wire = wire_sub_string(&sub_id);

    // Second subscribe (same skeleton, same relay): AuthorUnion widens the
    // SAME sub_id's filter to {a, e} -- an OVERWRITING REQ, snapshot2
    // absorbs {h_a, h_e}, pushed onto the SAME FIFO alongside snapshot1.
    let effects2 = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &e_key.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let (sub_id2, filter2) = req_for(&effects2, &relay0);
    assert_eq!(sub_id2, &sub_id, "same skeleton must reuse the sub id");
    assert_eq!(
        filter2.authors,
        Some(BTreeSet::from([
            a.public_key().to_hex(),
            e_key.public_key().to_hex()
        ]))
    );

    // A straggler EOSE for the sub now arrives, while BOTH snapshots are
    // outstanding.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(100u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire),
    ));

    let atom_a = cf(&[1], &[&a.public_key().to_hex()]);
    let atom_e = cf(&[1], &[&e_key.public_key().to_hex()]);
    assert!(
        core.get_coverage(&ctx_atom(atom_a.clone()), &relay0)
            .is_some(),
        "a is in BOTH outstanding snapshots -- must be credited"
    );
    assert!(
        core.get_coverage(&ctx_atom(atom_e.clone()), &relay0)
            .is_none(),
        "e is only in the newer snapshot -- the straggler EOSE must NOT credit it"
    );

    // The next EOSE (for the newer, still-outstanding snapshot) credits e.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(200u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire),
    ));
    assert!(
        core.get_coverage(&ctx_atom(atom_e.clone()), &relay0)
            .is_some(),
        "the second EOSE must credit the still-outstanding snapshot's atoms"
    );
}

// ---- limit poisons coverage ----------------------------------------------

#[test]
fn limited_fetch_never_records_coverage() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let limited_query = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([a.public_key().to_hex()]))),
        limit: Some(500),
        ..Filter::default()
    });
    let effects = core.handle(EngineMsg::Subscribe(
        limited_query,
        Box::new(CapturingSink::default()),
    ));
    let (sub_id, filter) = req_for(&effects, &relay0);
    assert_eq!(filter.limit, Some(500));
    let wire = wire_sub_string(sub_id);

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(500u64)));
    let _ = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire),
    ));

    let atom = cf(&[1], &[&a.public_key().to_hex()]);
    assert_eq!(
        core.get_coverage(&ctx_atom(atom.clone()), &relay0),
        None,
        "a limited REQ's EOSE must poison -- never record a watermark"
    );
}

// ---- per-source acquisition evidence (docs/design/
// scoped-evidence-49-12-plan.md §2/§3, folding #12 into #49) -------------

/// Find `relay`'s [`SourceEvidence`] entry, if any, inside `evidence`.
fn source_for<'a>(
    evidence: &'a AcquisitionEvidence,
    relay: &RelayUrl,
) -> Option<&'a SourceEvidence> {
    evidence.sources.iter().find(|s| &s.relay == relay)
}

fn evidence_from(effects: &[Effect], id: HandleId) -> Option<&AcquisitionEvidence> {
    effects.iter().find_map(|e| match e {
        Effect::EmitRows(hid, _, ev) if *hid == id => Some(ev),
        _ => None,
    })
}

#[test]
fn zero_atom_query_reports_no_resolved_demand_instead_of_vacuous_evidence() {
    let mut core = new_core(FixtureDirectory::new());
    let unresolved = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([9999u16])),
        authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
        ..Filter::default()
    });

    let effects = core.handle(EngineMsg::Subscribe(
        unresolved,
        Box::new(CapturingSink::default()),
    ));
    let evidence = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(_, _, evidence) => Some(evidence),
            _ => None,
        })
        .expect("a new subscription must emit its initial evidence");

    assert!(evidence.sources.is_empty());
    assert_eq!(evidence.shortfall, vec![ShortfallFact::NoResolvedDemand]);
}

#[test]
fn resolved_atom_without_a_planned_relay_reports_no_planned_source() {
    let a = Keys::generate();
    let atom = cf(&[9999], &[&a.public_key().to_hex()]);
    let mut core = new_core(FixtureDirectory::new());

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[9999], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let evidence = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(_, _, evidence) => Some(evidence),
            _ => None,
        })
        .expect("a new subscription must emit its initial evidence");

    assert!(evidence.sources.is_empty());
    assert_eq!(
        evidence.shortfall,
        vec![ShortfallFact::NoPlannedSource { atom }]
    );
}

#[test]
fn equal_evidence_on_reconnect_does_not_spuriously_emit_rows() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://stable-evidence.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);

    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[9999], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let first_connect = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 7,
            generation: 1,
        },
        public_session(&relay),
    ));
    assert!(
        first_connect
            .iter()
            .any(|effect| matches!(effect, Effect::EmitRows(..))),
        "Connecting -> Requesting is a real evidence change"
    );

    let unchanged_reconnect = core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 7,
            generation: 2,
        },
        public_session(&relay),
    ));
    assert!(
        unchanged_reconnect
            .iter()
            .all(|effect| !matches!(effect, Effect::EmitRows(..))),
        "deterministically equal source evidence must not produce a duplicate row batch"
    );
}

#[test]
fn surviving_handle_evidence_tracks_plan_changes_from_other_handle_lifetimes() {
    let a = Keys::generate();
    let b = Keys::generate();
    let r1 = RelayUrl::parse("wss://r1.example.com").unwrap();
    let r2 = RelayUrl::parse("wss://r2.example.com").unwrap();
    let r3 = RelayUrl::parse("wss://r3.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [r2.clone(), r3.clone()])
        .with_write(b.public_key().to_hex(), [r1.clone(), r2.clone()]);
    let mut core = EngineCore::new(MemoryStore::new(), Box::new(dir), 2);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[9999], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let a_id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, _, _) => Some(*id),
            _ => None,
        })
        .unwrap();
    let a_initial = evidence_from(&effects, a_id).unwrap();
    assert_eq!(
        a_initial
            .sources
            .iter()
            .map(|source| source.relay.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([r2.clone(), r3.clone()])
    );

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[9999], &b.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let b_id = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitRows(id, _, _) if *id != a_id => Some(*id),
            _ => None,
        })
        .next()
        .expect("the second subscription must emit its own initial batch");
    let a_while_b_is_live = evidence_from(&effects, a_id)
        .expect("adding B changes A's capped current plan and must refresh A");
    assert_eq!(
        a_while_b_is_live
            .sources
            .iter()
            .map(|source| source.relay.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([r2.clone()]),
        "the shared r2 plus lexicographically earlier r1 exhaust the cap while B is live"
    );

    let effects = core.handle(EngineMsg::Unsubscribe(b_id));
    let a_after_b_is_removed = evidence_from(&effects, a_id)
        .expect("removing B frees cap for r3 and must refresh surviving A");
    assert_eq!(
        a_after_b_is_removed
            .sources
            .iter()
            .map(|source| source.relay.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([r2, r3])
    );
}

/// The direct #12 fix falsifier: two independently-covering relays for the
/// SAME query never collapse into one verdict -- each relay's own proof (or
/// lack of it) is visible on its own `SourceEvidence` entry. Replaces the
/// deleted `QueryCoverage::CompleteUpTo`/`Unknown` unanimity test: there is
/// no aggregate here for either relay to jointly satisfy or fail.
#[test]
fn per_source_evidence_reflects_each_relays_own_proof_independently() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone(), relay1.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);
    connect(&mut core, 1, &relay1);

    let sink = CapturingSink::default();
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(sink.clone()),
    ));
    let id = effects
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(hid, ..) => Some(*hid),
            _ => None,
        })
        .expect("subscribe must emit an initial EmitRows for its own handle");
    let (sub0, _) = req_for(&effects, &relay0);
    let (sub1, _) = req_for(&effects, &relay1);
    let wire0 = wire_sub_string(sub0);
    let wire1 = wire_sub_string(sub1);

    // Only relay0 finishes: its OWN source flips to a proven watermark;
    // relay1's source stays unproven -- independently, no joint verdict.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(10u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire0),
    ));
    let evidence = evidence_from(&effects, id).expect("watermark advance must emit EmitRows");
    let r0 = source_for(evidence, &relay0).expect("relay0 must be a source");
    assert_eq!(r0.reconciled_through, Some(Timestamp::from(10u64)));
    let r1 = source_for(evidence, &relay1).expect("relay1 must be a source");
    assert_eq!(
        r1.reconciled_through, None,
        "relay1 has proven nothing yet -- its OWN entry must say so independently of relay0"
    );

    // relay1 also finishes: NOW its own entry advances too, still separate.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(20u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        public_session(&relay1),
        eose_frame(&wire1),
    ));
    let evidence = evidence_from(&effects, id).expect("watermark advance must emit EmitRows");
    let r1 = source_for(evidence, &relay1).expect("relay1 must be a source");
    assert_eq!(r1.reconciled_through, Some(Timestamp::from(20u64)));
}

/// #12's own falsifier, reshaped for the deleted-collapse model: a
/// `Derived` query ($myFollows shape) whose OUTER atom (kind:1 by the
/// followed author) has a proven coverage row, while the INNER atom (kind:3
/// -- the follow list itself, by the active identity) has none. The old
/// `query_coverage` consulted `root_atoms` ONLY, so the inner atom was
/// invisible to it and the query could report itself `CompleteUpTo` while
/// the follow-list expansion was entirely unproven. Under
/// `AcquisitionEvidence` (built over `subtree_atoms`, #12), the inner atom's
/// covering relay is its OWN source entry, unproven independently of the
/// outer relay's proof -- no field anywhere implies the feed is settled.
#[test]
fn derived_query_evidence_surfaces_the_unproven_inner_atom_independently_of_the_outer() {
    let a = Keys::generate();
    let b = Keys::generate();
    // relay0 hosts `a`'s own kind:3 (the inner/follow-list atom); relay1
    // hosts `b`'s kind:1 posts (the outer/root atom, once `a` follows `b`).
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay0.clone()])
        .with_write(b.public_key().to_hex(), [relay1.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);
    connect(&mut core, 1, &relay1);

    let my_follows = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Derived(Box::new(nmp_grammar::Derived {
            inner: nmp_grammar::Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([3u16])),
                authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
                ..Filter::default()
            }),
            project: nmp_grammar::Selector::Tag("p".to_string()),
        }))),
        ..Filter::default()
    });

    let _ = core.handle(EngineMsg::SetActivePubkey(Some(a.public_key())));
    let effects = core.handle(EngineMsg::Subscribe(
        my_follows,
        Box::new(CapturingSink::default()),
    ));
    let id = effects
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(hid, ..) => Some(*hid),
            _ => None,
        })
        .expect("subscribe must emit an initial EmitRows for its own handle");
    // Only the inner atom (kind:3 by `a`) is resolvable at subscribe time --
    // the outer author set is still empty (no wildcard), so relay0 is the
    // only wire sub open right now.
    let (sub0, _) = req_for_kind(&effects, &relay0, 3);
    let wire0 = wire_sub_string(sub0);

    // `a` follows `b`: the outer atom {kind:1, authors:{b}} now resolves and
    // opens relay1.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(10u64)));
    let contact_list = nmp_resolver::testkit::kind3(&a, &[b.public_key()], 10);
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        event_frame(&wire0, contact_list),
    ));
    // #11: the source relay is also projected as provenance for the outer
    // author, so relay0 now carries a distinct kind:1 outer request too.
    let (outer0, _) = req_for_kind(&effects, &relay0, 1);
    let wire_outer0 = wire_sub_string(outer0);
    let (sub1, _) = req_for_kind(&effects, &relay1, 1);
    let wire1 = wire_sub_string(sub1);

    // The OUTER atom's relay (relay1) proves its window; the INNER atom's
    // relay (relay0, the follow-list itself) never gets an EOSE.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(20u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 1,
            generation: 1,
        },
        public_session(&relay1),
        eose_frame(&wire1),
    ));
    let evidence = evidence_from(&effects, id).expect("watermark advance must emit EmitRows");
    let outer = source_for(evidence, &relay1).expect("relay1 (outer) must be a source");
    assert_eq!(
        outer.reconciled_through,
        Some(Timestamp::from(20u64)),
        "the outer atom's own relay proved its own window"
    );
    let inner = source_for(evidence, &relay0).expect(
        "relay0 (the INNER kind:3 atom's covering relay) must be PRESENT in evidence.sources -- \
         the whole point of #12 is that interior atoms are consulted, never invisible",
    );
    assert_eq!(
        inner.reconciled_through, None,
        "the inner atom (the follow-list itself) has proven nothing -- no source anywhere may \
         imply this feed is settled while the follow-list expansion is unproven"
    );

    // The inner EOSE alone cannot flip relay0's aggregate source evidence:
    // #11 also routes the outer atom there from source provenance, and that
    // second relay0 request is still unproven.
    let _ = core.handle(EngineMsg::Tick(Timestamp::from(30u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire0),
    ));
    assert!(
        evidence_from(&effects, id).is_none(),
        "one of relay0's two current atoms remains unproven"
    );
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire_outer0),
    ));
    let evidence = evidence_from(&effects, id).expect("watermark advance must emit EmitRows");
    let inner = source_for(evidence, &relay0).expect("relay0 must still be a source");
    assert_eq!(inner.reconciled_through, Some(Timestamp::from(30u64)));
}

/// The orthogonality proof (docs/design/scoped-evidence-49-12-plan.md Q3):
/// a relay's durable watermark and its current link status are
/// INDEPENDENT fields, never one enum. A source that proved its window and
/// then dropped must keep reporting BOTH facts in the SAME snapshot --
/// `reconciled_through: Some(_)` (the #49 "offline cached rows remain
/// usable" acceptance criterion) AND `status: Disconnected`, simultaneously.
#[test]
fn source_watermark_survives_disconnect_alongside_the_disconnected_status() {
    let a = Keys::generate();
    let relay0 = RelayUrl::parse("wss://relay0.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay0.clone()]);
    let mut core = new_core(dir);
    connect(&mut core, 0, &relay0);

    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let id = effects
        .iter()
        .find_map(|e| match e {
            Effect::EmitRows(hid, ..) => Some(*hid),
            _ => None,
        })
        .expect("subscribe must emit an initial EmitRows for its own handle");
    let (sub0, _) = req_for(&effects, &relay0);
    let wire0 = wire_sub_string(sub0);

    let _ = core.handle(EngineMsg::Tick(Timestamp::from(10u64)));
    let effects = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        eose_frame(&wire0),
    ));
    let evidence = evidence_from(&effects, id).expect("watermark advance must emit EmitRows");
    let r0 = source_for(evidence, &relay0).expect("relay0 must be a source");
    assert_eq!(r0.reconciled_through, Some(Timestamp::from(10u64)));
    assert_eq!(r0.status, SourceStatus::Requesting);

    // relay0 drops. Its watermark must survive; its status must flip.
    let effects = core.handle(EngineMsg::RelayDisconnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        public_session(&relay0),
        DisconnectReason::Error,
    ));
    let evidence = evidence_from(&effects, id).expect("a link-status flip must emit EmitRows");
    let r0 = source_for(evidence, &relay0).expect("relay0 must still be a source");
    assert_eq!(
        r0.reconciled_through,
        Some(Timestamp::from(10u64)),
        "the prior watermark must survive a disconnect -- offline cached rows remain usable"
    );
    assert_eq!(
        r0.status,
        SourceStatus::Disconnected,
        "the link status must independently reflect the drop"
    );
}

/// #440: closing the last owner can synchronously release a pool slot while
/// a caller immediately creates fresh demand for the same relay. The slot is
/// then reused at a new generation before the old disconnect reaches the
/// reducer. That stale fact must not erase the reopened connection.
#[test]
fn stale_disconnect_cannot_erase_a_reopened_slot_generation() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://relay.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);
    let effects = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitRows(id, ..) => Some(*id),
            _ => None,
        })
        .expect("subscribe emits its initial row snapshot");

    let old = RelayHandle {
        slot: 0,
        generation: 1,
    };
    let reopened = RelayHandle {
        slot: 0,
        generation: 2,
    };
    let session = public_session(&relay);
    let _ = core.handle(EngineMsg::RelayConnected(old, session.clone()));
    let _ = core.handle(EngineMsg::RelayConnected(reopened, session.clone()));

    let stale_connect = core.handle(EngineMsg::RelayConnected(old, session.clone()));
    assert!(
        stale_connect.is_empty(),
        "an old-generation connect must not replace the reopened handle"
    );

    let stale_health = core.handle(EngineMsg::RelayHealth(
        old,
        session.clone(),
        nmp_transport::RelayHealth {
            last_error: Some("stale generation failed".to_string()),
            ..nmp_transport::RelayHealth::default()
        },
    ));
    assert!(
        stale_health.is_empty(),
        "old-generation health must not mutate reopened diagnostics"
    );
    assert!(core.diagnostics_snapshot().transport_degraded.is_none());

    let stale = core.handle(EngineMsg::RelayDisconnected(
        old,
        session.clone(),
        DisconnectReason::Error,
    ));
    assert!(
        stale.is_empty(),
        "an old-generation disconnect must be a reducer no-op"
    );

    let current = core.handle(EngineMsg::RelayDisconnected(
        reopened,
        session.clone(),
        DisconnectReason::Error,
    ));
    let evidence = evidence_from(&current, id).expect("the current disconnect refreshes evidence");
    assert_eq!(
        source_for(evidence, &relay)
            .expect("relay remains the planned source")
            .status,
        SourceStatus::Disconnected,
    );
    assert!(
        current
            .iter()
            .any(|effect| matches!(effect, Effect::EnsureRelay(key) if key == &session)),
        "the current generation disconnect still re-ensures required work"
    );
}

/// The CRITICAL falsifier (issue #506), reducer half: a
/// `DisconnectReason::PermanentlyFailed` (401/403 -- the transport pool has
/// ALREADY retired the worker and freed its cap slot by the time this
/// reaches the reducer) must NEVER re-issue `Effect::EnsureRelay` -- doing
/// so is either a no-op race against a wedged zombie (the pre-#506 bug) or,
/// since the pool now grants a fresh worker on any `ensure_open` against an
/// empty slot, a tight 401 busy-redial loop. It must instead record a
/// terminal degraded diagnostics fact (the same `transport_degraded` field
/// `on_relay_health` owns) so the failure stays OBSERVABLE without the
/// reducer ever trying again on its own.
#[test]
fn permanently_failed_relay_never_re_ensures_and_records_terminal_diagnostics() {
    let a = Keys::generate();
    let relay = RelayUrl::parse("wss://relay.example.com").unwrap();
    let dir = FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay.clone()]);
    let mut core = new_core(dir);
    let _ = core.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));

    let handle = RelayHandle {
        slot: 0,
        generation: 1,
    };
    let _ = core.handle(EngineMsg::RelayConnected(handle, public_session(&relay)));
    assert!(core.diagnostics_snapshot().transport_degraded.is_none());

    let effects = core.handle(EngineMsg::RelayDisconnected(
        handle,
        public_session(&relay),
        DisconnectReason::PermanentlyFailed,
    ));

    assert!(
        !effects.iter().any(
            |effect| matches!(effect, Effect::EnsureRelay(url) if url == &public_session(&relay))
        ),
        "a permanent failure must never re-issue EnsureRelay -- the pool has \
         already retired this worker for good, so this would either race a \
         wedged zombie or busy-loop redialing a relay that keeps refusing"
    );
    let degraded = core
        .diagnostics_snapshot()
        .transport_degraded
        .expect("a permanent failure must record a terminal degraded diagnostics fact");
    assert!(
        degraded.contains(relay.as_str()),
        "the degraded fact should identify which relay permanently failed, got: {degraded}"
    );

    // Contrast: the ORDINARY (transient) reason on an otherwise identical
    // setup keeps re-issuing EnsureRelay exactly as before -- the fix must
    // not touch that path at all.
    let mut core_transient =
        new_core(FixtureDirectory::new().with_write(a.public_key().to_hex(), [relay.clone()]));
    let _ = core_transient.handle(EngineMsg::Subscribe(
        literal_query(&[1], &a.public_key().to_hex()),
        Box::new(CapturingSink::default()),
    ));
    let _ = core_transient.handle(EngineMsg::RelayConnected(handle, public_session(&relay)));
    let transient_effects = core_transient.handle(EngineMsg::RelayDisconnected(
        handle,
        public_session(&relay),
        DisconnectReason::Error,
    ));
    assert!(
        transient_effects.iter().any(
            |effect| matches!(effect, Effect::EnsureRelay(url) if url == &public_session(&relay))
        ),
        "an ordinary transient disconnect must keep re-issuing EnsureRelay unchanged"
    );
    assert!(
        core_transient
            .diagnostics_snapshot()
            .transport_degraded
            .is_none(),
        "an ordinary transient disconnect must not fabricate a terminal degraded fact"
    );
}

// ---- set-active-pubkey re-root ------------------------------------------

#[test]
fn set_active_pubkey_reroots_and_recompiles() {
    let a = Keys::generate();
    let b = Keys::generate();
    let relay_a = RelayUrl::parse("wss://relay-a.example.com").unwrap();
    let relay_b = RelayUrl::parse("wss://relay-b.example.com").unwrap();
    let dir = FixtureDirectory::new()
        .with_write(a.public_key().to_hex(), [relay_a.clone()])
        .with_write(b.public_key().to_hex(), [relay_b.clone()]);
    let mut core = new_core(dir);

    let whoami = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([0u16])),
        authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
        ..Filter::default()
    });

    let _ = core.handle(EngineMsg::SetActivePubkey(Some(a.public_key())));
    let effects = core.handle(EngineMsg::Subscribe(
        whoami,
        Box::new(CapturingSink::default()),
    ));
    req_for(&effects, &relay_a); // demand is currently for `a`.

    let effects = core.handle(EngineMsg::SetActivePubkey(Some(b.public_key())));
    let closed_a = effects.iter().any(|e| {
        matches!(e, Effect::Wire(d) if d.ops.iter().any(|(r, ops)| r.relay == relay_a && ops.iter().any(|op| matches!(op, WireOp::Close(_)))))
    });
    assert!(closed_a, "re-root must close a's demand");
    req_for(&effects, &relay_b); // and open b's.
}
