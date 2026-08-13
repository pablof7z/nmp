use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use nmp::nip29::GroupAvailability;
use nmp::{
    nip29, AccessContext, Binding, CacheMode, Demand, Derived, DiagnosticsSnapshot, Engine,
    EngineConfig, EventBuilder, Filter, IdentityField, Kind, PublicKey, RelayState, RelayUrl,
    Selector, SourceAuthority, SourceStatus, Tag, Window, WriteFact,
};

use crate::args::{Args, Mode};
use crate::observe::{wait_until, Observed};

const GROUP: &str = "bitcoin";
const SINGLE_GROUP: &str = "solo-a";
const MIXED_GROUP: &str = "one-sided";
const SHARED_CHAT: &str = "shared chat observed at both hosts";
const RELAY_B_CHAT: &str = "relay B chat";
const LIVE_CHAT: &str = "shared live chat after sibling cancellation";

pub fn run(args: Args) -> Result<(), String> {
    match args.mode {
        Mode::Online => online(args),
        Mode::LiveAdversarial => live_adversarial(args),
        Mode::ProvenanceGrowth => provenance_growth(args),
        Mode::Restart => restart(args),
        Mode::RestartConflict => restart_conflict(args),
    }
}

struct Context {
    engine: Engine,
    relay_a: RelayUrl,
    relay_b: RelayUrl,
    viewer: PublicKey,
    followed: PublicKey,
    outsider: PublicKey,
    writer: PublicKey,
    _writer_account: nmp::SessionAccount,
    settle: Duration,
}

impl Context {
    fn open(args: &Args) -> Result<Self, String> {
        let relay_a = parse_relay("relay A", &args.relay_a)?;
        let relay_b = parse_relay("relay B", &args.relay_b)?;
        let viewer = PublicKey::parse(&args.viewer)
            .map_err(|error| format!("invalid viewer pubkey: {error}"))?;
        let followed = PublicKey::parse(&args.followed)
            .map_err(|error| format!("invalid followed pubkey: {error}"))?;
        let outsider = PublicKey::parse(&args.outsider)
            .map_err(|error| format!("invalid outsider pubkey: {error}"))?;
        let store_path = args
            .store_path
            .to_str()
            .ok_or_else(|| "store path is not UTF-8".to_string())?
            .to_string();
        let engine = Engine::new(EngineConfig {
            store_path: Some(store_path),
            max_relays: 4,
            ..EngineConfig::default()
        })
        .map_err(|error| format!("engine construction failed: {error}"))?;
        engine
            .add_public_key_account(viewer, true)
            .map_err(|error| format!("current viewer installation failed: {error}"))?;

        let mut secret = read_secret_key(&args.writer_secret_file)?;
        let account_result = engine.add_private_key_account(&secret, false);
        secret.fill(0);
        let account = account_result
            .map_err(|error| format!("writer account installation failed: {error}"))?;
        let writer = account.public_key();

        Ok(Self {
            engine,
            relay_a,
            relay_b,
            viewer,
            followed,
            outsider,
            writer,
            _writer_account: account,
            settle: Duration::from_secs(args.settle_secs),
        })
    }

    fn scope(&self) -> Result<nip29::RelayScope, String> {
        nip29::on([self.relay_a.clone(), self.relay_b.clone()])
            .map_err(|error| format!("relay scope refused: {error}"))
    }

    fn shutdown(self) {
        self.engine.shutdown();
    }
}

fn read_secret_key(path: &std::path::Path) -> Result<[u8; 32], String> {
    let mut encoded =
        fs::read(path).map_err(|error| format!("could not read writer secret file: {error}"))?;
    let result = decode_hex_secret(&encoded);
    encoded.fill(0);
    result
}

fn decode_hex_secret(encoded: &[u8]) -> Result<[u8; 32], String> {
    let encoded = encoded.strip_suffix(b"\n").unwrap_or(encoded);
    let encoded = encoded.strip_suffix(b"\r").unwrap_or(encoded);
    if encoded.len() != 64 {
        return Err("writer secret file must contain one 64-character hex key".to_string());
    }

    let mut secret = [0_u8; 32];
    let (pairs, remainder) = encoded.as_chunks::<2>();
    debug_assert!(remainder.is_empty());
    for (index, pair) in pairs.iter().enumerate() {
        let (high, low) = match (hex_nibble(pair[0]), hex_nibble(pair[1])) {
            (Ok(high), Ok(low)) => (high, low),
            _ => {
                secret.fill(0);
                return Err("writer secret file contains non-hex input".to_string());
            }
        };
        secret[index] = (high << 4) | low;
    }
    Ok(secret)
}

fn hex_nibble(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err("not a hex digit".to_string()),
    }
}

fn online(args: Args) -> Result<(), String> {
    let context = Context::open(&args)?;
    let scope = context.scope()?;
    let group = scope.group(GROUP);

    let single_scope = nip29::on([context.relay_a.clone()])
        .map_err(|error| format!("single relay scope refused: {error}"))?;
    let single = single_scope.group(SINGLE_GROUP);
    let single_subscription = context
        .engine
        .observe(single.read(kinds([9])).map_err(display)?, None)
        .map_err(display)?;
    let mut single_rows = Observed::default();
    wait_until(
        &single_subscription,
        context.settle,
        &mut single_rows,
        |observed| observed.rows_of_kind(9).len() == 1,
    )?;
    ensure(
        single_rows
            .rows
            .values()
            .all(|row| row.sources == BTreeSet::from([context.relay_a.clone()])),
        "single-host group leaked a source other than relay A",
    )?;
    println!(
        "PROOF single_host_group kind9_rows=1 source={}",
        context.relay_a
    );

    let chat_subscription = context
        .engine
        .observe(group.read(kinds([9])).map_err(display)?, None)
        .map_err(display)?;
    std::thread::sleep(Duration::from_millis(1_200));
    let mut chats = Observed::default();
    wait_until(&chat_subscription, context.settle, &mut chats, |observed| {
        observed.rows_of_kind(9).len() >= 27 && observed.has_source_count(SHARED_CHAT, 2)
    })?;
    ensure(
        chats.rows_of_kind(9).len() == 27,
        format!(
            "expected 27 distinct seeded kind 9 rows, saw {}",
            chats.rows_of_kind(9).len()
        ),
    )?;
    println!(
        "PROOF kind9 distinct=27 shared_sources=2 slow_consumer_frames={} delta_entries={}",
        chats.frames, chats.delta_entries
    );

    let article_subscription = context
        .engine
        .observe(group.read(kinds([30023])).map_err(display)?, None)
        .map_err(display)?;
    let mut articles = Observed::default();
    wait_until(
        &article_subscription,
        context.settle,
        &mut articles,
        |observed| {
            observed.rows_of_kind(30023).len() == 3
                && observed.has_source_count("shared long-form event", 2)
        },
    )?;
    println!("PROOF kind30023 distinct=3 shared_sources=2");

    verify_metadata_conflict(&context, &scope)?;
    verify_follows_discovery(&context, &scope)?;
    verify_window(&context, &group)?;
    verify_diagnostics(&context)?;

    drop(single_subscription);
    drop(chat_subscription);
    drop(article_subscription);

    verify_publications(&context, &scope)?;
    println!("PASS online");
    context.shutdown();
    Ok(())
}

fn verify_metadata_conflict(context: &Context, scope: &nip29::RelayScope) -> Result<(), String> {
    let watching = metadata_observation(context, scope)?;
    let snapshots = wait_for_snapshots(context, &watching, "metadata conflict", |snapshots| {
        bitcoin(snapshots).is_some_and(|snapshot| snapshot.per_host.len() == 2)
    })?;
    let snapshot = bitcoin(&snapshots).expect("the predicate proved it is there");
    let names_by_source = metadata_names(snapshot)?;
    ensure(
        names_by_source.get(&context.relay_a).map(String::as_str) == Some("Bitcoin Cash")
            && names_by_source.get(&context.relay_b).map(String::as_str) == Some("Bitcoin (real)"),
        format!("conflicting relay metadata was not preserved: {names_by_source:?}"),
    )?;
    // The aggregate is ONE relay's whole record, not a blend of the two, and
    // the app is told the two disagree so it can decide what to show.
    let shown = snapshot
        .metadata
        .as_ref()
        .ok_or_else(|| "no metadata was shown at all".to_string())?;
    ensure(
        names_by_source
            .values()
            .any(|name| Some(name.as_str()) == shown.name.as_deref()),
        format!("the shown name {:?} is not either relay's own", shown.name),
    )?;
    ensure(
        snapshot.differs(nip29::GroupRecord::Metadata),
        "two relays published different metadata and the app was not told they disagree",
    )?;
    let display_winner = names_by_source
        .get(&context.relay_b)
        .expect("relay B's asserted row exists");
    println!(
        "PROOF metadata_conflict preserved=2 shown_host={} shown_name={:?} differs=true \
         app_winner={display_winner:?} policy=prefer_relay_b",
        shown.host, shown.name
    );
    Ok(())
}

fn verify_follows_discovery(context: &Context, scope: &nip29::RelayScope) -> Result<(), String> {
    let watching = follows_discovery_observation(context, scope)?;
    let snapshots = wait_for_snapshots(context, &watching, "follows discovery", |snapshots| {
        bitcoin(snapshots).is_some_and(|snapshot| lists_followed(snapshot, &context.followed))
    })?;
    ensure(
        snapshots.len() == 1,
        format!(
            "follows-derived discovery returned another group: {:?}",
            snapshots.iter().map(|s| s.id.clone()).collect::<Vec<_>>()
        ),
    )?;
    let snapshot = bitcoin(&snapshots).expect("the predicate proved it is there");
    ensure(
        snapshot.per_host.keys().cloned().collect::<Vec<_>>() == vec![context.relay_a.clone()],
        "follows-derived discovery crossed relay authority",
    )?;
    println!(
        "PROOF discovery predicate=member_list_includes(follows_of_active_viewer) group={GROUP} viewer={} followed={} evidence_source={}",
        context.viewer, context.followed, context.relay_a
    );
    Ok(())
}

/// The member list NMP handed back names the followed subject -- read off the
/// typed snapshot, with no `p`-tag walking anywhere in this application.
fn lists_followed(snapshot: &nip29::GroupSnapshot, followed: &PublicKey) -> bool {
    snapshot
        .members
        .iter()
        .any(|subject| &subject.pubkey == followed)
}

fn metadata_observation(
    context: &Context,
    scope: &nip29::RelayScope,
) -> Result<nip29::GroupObservation, String> {
    // Relay-signed group records are keyed by `d`, unlike group content's `h`
    // -- reaching for them through the content door is refused outright
    // (#1245). Positive member-list evidence at each relay selects the group.
    let subjects = Binding::Literal(BTreeSet::from([
        context.followed.to_hex(),
        context.outsider.to_hex(),
    ]));
    scope
        .observe(
            &context.engine,
            nip29::member_list_includes(subjects),
            [nip29::GroupRecord::Metadata],
            None,
        )
        .map_err(display)
}

/// Exactly what each relay signed, read off the per-host breakdown beside the
/// aggregate -- no tag walking, and no relay's record folded into another's.
fn metadata_names(snapshot: &nip29::GroupSnapshot) -> Result<BTreeMap<RelayUrl, String>, String> {
    snapshot
        .per_host
        .iter()
        .map(|(host, records)| {
            let name = records
                .metadata
                .as_ref()
                .and_then(|record| record.name.clone())
                .ok_or_else(|| format!("relay {host} published no group name"))?;
            Ok((host.clone(), name))
        })
        .collect()
}

fn follows_discovery_observation(
    context: &Context,
    scope: &nip29::RelayScope,
) -> Result<nip29::GroupObservation, String> {
    let mut follows_demand = Demand::new(
        Filter {
            kinds: Some(BTreeSet::from([3])),
            authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
            ..Filter::default()
        },
        SourceAuthority::Pinned(BTreeSet::from([
            context.relay_a.clone(),
            context.relay_b.clone(),
        ])),
        AccessContext::Public,
    )
    .map_err(display)?;
    follows_demand.cache = CacheMode::Strict;
    let follows = Binding::Derived(Box::new(Derived {
        inner: follows_demand,
        project: Selector::Tag("p".to_string()),
    }));
    scope
        .observe(
            &context.engine,
            nip29::member_list_includes(follows),
            [nip29::GroupRecord::Members],
            None,
        )
        .map_err(display)
}

/// Await the records observation from this synchronous probe.
///
/// The probe deliberately depends on `nmp` and nothing else, so it borrows the
/// engine's own runtime handle rather than growing an async runtime dependency
/// of its own. `block_on` here runs on the probe's main thread, which is never
/// one of that runtime's workers.
fn wait_for_snapshots(
    context: &Context,
    watching: &nip29::GroupObservation,
    what: &str,
    pred: impl Fn(&[nip29::GroupSnapshot]) -> bool,
) -> Result<Vec<nip29::GroupSnapshot>, String> {
    let runtime = context.engine.adapter_runtime().map_err(display)?;
    let deadline = Instant::now() + context.settle;
    runtime.block_on(async move {
        let mut last: Vec<nip29::GroupSnapshot> = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "{what}: no delivery satisfied the condition; last saw {} group(s)",
                    last.len()
                ));
            }
            match watching.next_within(remaining).await {
                Ok(Some(snapshots)) => {
                    last = snapshots;
                    if pred(&last) {
                        return Ok(last);
                    }
                }
                Ok(None) => return Err(format!("{what}: the observation was withdrawn")),
                Err(error) => return Err(format!("{what}: {error}")),
            }
        }
    })
}

/// The one group the probe watches, if the delivery names it.
fn bitcoin(snapshots: &[nip29::GroupSnapshot]) -> Option<&nip29::GroupSnapshot> {
    snapshots.iter().find(|snapshot| snapshot.id == GROUP)
}

fn verify_window(context: &Context, group: &nip29::Group) -> Result<(), String> {
    let window = Window::Expandable {
        initial: NonZeroUsize::new(3).expect("three is nonzero"),
        max: NonZeroUsize::new(8).expect("eight is nonzero"),
    };
    let subscription = context
        .engine
        .observe(group.read(kinds([9])).map_err(display)?, Some(window))
        .map_err(display)?;
    std::thread::sleep(Duration::from_millis(800));
    let first = subscription
        .recv_timeout(context.settle)
        .map_err(|error| format!("initial window did not arrive: {error:?}"))?;
    let initial = first
        .window
        .ok_or_else(|| "windowed observation returned no window".to_string())?;
    ensure(
        initial.rows.len() == 3,
        format!("initial window held {} rows", initial.rows.len()),
    )?;
    subscription.request_rows(8).map_err(display)?;
    let grown = loop {
        let frame = subscription
            .recv_timeout(context.settle)
            .map_err(|error| format!("grown window did not arrive: {error:?}"))?;
        if let Some(contents) = frame.window {
            if contents.rows.len() == 8 {
                break contents;
            }
        }
    };
    println!(
        "PROOF window initial=3 grown={} max=8 load={:?}",
        grown.rows.len(),
        grown.load
    );
    Ok(())
}

fn verify_diagnostics(context: &Context) -> Result<(), String> {
    let diagnostics = context.engine.observe_diagnostics().map_err(display)?;
    let snapshot = diagnostics
        .recv()
        .ok_or_else(|| "diagnostics stream ended before its current snapshot".to_string())?;
    let relevant: Vec<_> = snapshot
        .relays
        .iter()
        .filter(|relay| relay.relay == context.relay_a || relay.relay == context.relay_b)
        .collect();
    ensure(
        relevant.len() == 2,
        format!(
            "expected diagnostics for two relays, saw {}",
            relevant.len()
        ),
    )?;
    for relay in relevant {
        ensure(
            relay.wire_sub_count > 0,
            format!("{} has no wire subscriptions", relay.relay),
        )?;
        ensure(
            relay
                .filters
                .iter()
                .any(|filter| filter.contains("#h") && filter.contains(GROUP)),
            format!(
                "{} exposes no exact group filter: {:?}",
                relay.relay, relay.filters
            ),
        )?;
        println!(
            "PROOF diagnostics relay={} wire_sub_count={} filters={:?} events_by_kind={:?} coverage={:?}",
            relay.relay, relay.wire_sub_count, relay.filters, relay.events_by_kind, relay.coverage
        );
    }
    Ok(())
}

fn verify_publications(context: &Context, scope: &nip29::RelayScope) -> Result<(), String> {
    let group = scope.group(GROUP);
    let chat = group
        .publish(
            &context.engine,
            context.writer,
            EventBuilder::new(Kind::from(9u16)).content("NMP consumer published chat"),
        )
        .map_err(display)?;
    let chat_statuses = wait_for_write(&chat.statuses, context.settle, |statuses| {
        acked(statuses, &context.relay_a) && acked(statuses, &context.relay_b)
    })?;
    println!("PROOF publish kind=9 outcomes={chat_statuses:?}");

    let article = group
        .publish(
            &context.engine,
            context.writer,
            EventBuilder::new(Kind::from(30023u16))
                .tag(Tag::parse(["d", "nmp-consumer-article"]).map_err(display)?)
                .content("NMP consumer published long-form event"),
        )
        .map_err(display)?;
    let article_statuses = wait_for_write(&article.statuses, context.settle, |statuses| {
        acked(statuses, &context.relay_a) && acked(statuses, &context.relay_b)
    })?;
    println!("PROOF publish kind=30023 outcomes={article_statuses:?}");

    let mixed = scope
        .group(MIXED_GROUP)
        .publish(
            &context.engine,
            context.writer,
            EventBuilder::new(Kind::from(9u16)).content("NMP mixed-outcome publication"),
        )
        .map_err(display)?;
    let mixed_statuses = wait_for_write(&mixed.statuses, context.settle, |statuses| {
        acked(statuses, &context.relay_a)
            && statuses.iter().any(
                |status| matches!(status, WriteFact::Relay { relay, state: RelayState::Rejected { .. } } if relay == &context.relay_b),
            )
    })?;
    println!("PROOF publish mixed_group={MIXED_GROUP} outcomes={mixed_statuses:?}");
    Ok(())
}

fn live_adversarial(args: Args) -> Result<(), String> {
    let context = Context::open(&args)?;
    let scope = context.scope()?;
    let group = scope.group(GROUP);

    let metadata_watch = metadata_observation(&context, &scope)?;
    wait_for_snapshots(&context, &metadata_watch, "initial metadata", |snapshots| {
        bitcoin(snapshots).is_some_and(|snapshot| snapshot.per_host.len() == 2)
    })?;

    let discovery_watch = follows_discovery_observation(&context, &scope)?;
    wait_for_snapshots(
        &context,
        &discovery_watch,
        "initial discovery",
        |snapshots| {
            bitcoin(snapshots).is_some_and(|snapshot| lists_followed(snapshot, &context.followed))
        },
    )?;

    let chat_query = group.read(kinds([9])).map_err(display)?;
    let cancelled_subscription = context
        .engine
        .observe(chat_query.clone(), None)
        .map_err(display)?;
    let surviving_subscription = context.engine.observe(chat_query, None).map_err(display)?;
    let mut cancelled_rows = Observed::default();
    let mut surviving_rows = Observed::default();
    wait_until(
        &cancelled_subscription,
        context.settle,
        &mut cancelled_rows,
        |state| state.rows_of_kind(9).len() >= 27,
    )?;
    wait_until(
        &surviving_subscription,
        context.settle,
        &mut surviving_rows,
        |state| state.rows_of_kind(9).len() >= 27,
    )?;

    let shared_wire = wait_for_group_filter_counts(&context, 1)?;
    drop(cancelled_subscription);
    let after_one_cancel = wait_for_group_filter_counts(&context, 1)?;
    ensure(
        shared_wire == after_one_cancel,
        format!(
            "cancelling one shared observation changed the surviving wire demand: before={shared_wire:?} after={after_one_cancel:?}"
        ),
    )?;

    stage_round_trip(&args, "mutate-live-inputs")?;
    let mutated = wait_for_snapshots(&context, &metadata_watch, "mutated metadata", |snapshots| {
        bitcoin(snapshots).is_some_and(|snapshot| {
            metadata_names(snapshot).is_ok_and(|names| {
                names.get(&context.relay_a).map(String::as_str) == Some("Bitcoin Cash live")
                    && names.get(&context.relay_b).map(String::as_str)
                        == Some("Bitcoin (real) live")
            })
        })
    })?;
    wait_until(
        &surviving_subscription,
        context.settle,
        &mut surviving_rows,
        |state| state.has_source_count(LIVE_CHAT, 2),
    )?;
    // The follow is withdrawn, so the evidence that put this group in the
    // listing is gone and the group leaves the listing. Absence here is the
    // predicate no longer matching, never a claim about membership.
    wait_for_snapshots(
        &context,
        &discovery_watch,
        "follow withdrawn",
        |snapshots| {
            !bitcoin(snapshots).is_some_and(|snapshot| lists_followed(snapshot, &context.followed))
        },
    )?;
    println!(
        "PROOF live_mutation metadata={:?} follows_removed=true surviving_chat_sources=2 shared_wire={after_one_cancel:?}",
        metadata_names(bitcoin(&mutated).expect("the predicate proved it is there"))?
    );

    stage_round_trip(&args, "restore-follow")?;
    wait_for_snapshots(&context, &discovery_watch, "follow restored", |snapshots| {
        bitcoin(snapshots).is_some_and(|snapshot| lists_followed(snapshot, &context.followed))
    })?;
    println!("PROOF live_follow_readded group={GROUP} observation_reused=true");

    drop(surviving_subscription);
    let after_last_cancel = wait_for_group_filter_counts(&context, 0)?;
    println!(
        "PROOF shared_cancellation before={shared_wire:?} after_one={after_one_cancel:?} after_last={after_last_cancel:?}"
    );
    println!("PASS live-adversarial");
    drop(metadata_watch);
    drop(discovery_watch);
    context.shutdown();
    Ok(())
}

fn wait_for_group_filter_counts(
    context: &Context,
    expected_per_relay: usize,
) -> Result<BTreeMap<RelayUrl, usize>, String> {
    let deadline = Instant::now() + context.settle;
    loop {
        let snapshot = context
            .engine
            .observe_diagnostics()
            .map_err(display)?
            .recv()
            .ok_or_else(|| "diagnostics ended before its current snapshot".to_string())?;
        let counts = group_filter_counts(&snapshot, context);
        if counts.len() == 2 && counts.values().all(|count| *count == expected_per_relay) {
            return Ok(counts);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "group wire-filter count did not reach {expected_per_relay} per relay: {counts:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn group_filter_counts(
    snapshot: &DiagnosticsSnapshot,
    context: &Context,
) -> BTreeMap<RelayUrl, usize> {
    snapshot
        .relays
        .iter()
        .filter(|relay| relay.relay == context.relay_a || relay.relay == context.relay_b)
        .map(|relay| {
            let count = relay
                .filters
                .iter()
                .filter(|filter| {
                    filter.contains("\"kinds\":[9]")
                        && filter.contains("#h")
                        && filter.contains(GROUP)
                })
                .count();
            (relay.relay.clone(), count)
        })
        .collect()
}

fn stage_round_trip(args: &Args, name: &str) -> Result<(), String> {
    let stage_dir = args
        .stage_dir
        .as_ref()
        .ok_or_else(|| format!("{name} requires --stage-dir"))?;
    fs::create_dir_all(stage_dir).map_err(|error| {
        format!(
            "could not create stage directory {}: {error}",
            stage_dir.display()
        )
    })?;
    let ready = stage_dir.join(format!("{name}.ready"));
    let proceed = stage_dir.join(format!("{name}.continue"));
    fs::write(&ready, b"ready\n")
        .map_err(|error| format!("could not write stage file {}: {error}", ready.display()))?;
    let deadline = Instant::now() + Duration::from_secs(args.settle_secs.saturating_mul(2).max(1));
    while !proceed.is_file() {
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for stage continuation {}",
                proceed.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn provenance_growth(args: Args) -> Result<(), String> {
    let context = Context::open(&args)?;
    let group = context.scope()?.group(GROUP);
    let subscription = context
        .engine
        .observe(group.read(kinds([9])).map_err(display)?, None)
        .map_err(display)?;
    let mut observed = Observed::default();
    wait_until(&subscription, context.settle, &mut observed, |state| {
        state.has_source_count(SHARED_CHAT, 1)
            && state
                .rows
                .values()
                .any(|row| row.content() == "relay A chat")
    })?;
    ensure(
        !observed
            .rows
            .values()
            .any(|row| row.content() == RELAY_B_CHAT),
        "relay B-only content arrived while relay B was staged down",
    )?;
    signal_ready(&args)?;
    println!("PROOF provenance_before shared_sources=1 relay_b_content=false");

    wait_until(&subscription, context.settle, &mut observed, |state| {
        state.has_source_count(SHARED_CHAT, 2)
            && state.rows.values().any(|row| row.content() == RELAY_B_CHAT)
    })?;
    let shared = observed
        .rows
        .values()
        .find(|row| row.content() == SHARED_CHAT)
        .expect("shared row exists after predicate");
    ensure(
        observed.source_growth.contains(&shared.id()),
        "the existing shared row reached two sources without SourcesGrew",
    )?;
    println!(
        "PROOF provenance_after shared_sources=2 relay_b_content=true sources_grew_event={}",
        shared.id()
    );
    println!("PASS provenance-growth");
    drop(subscription);
    context.shutdown();
    Ok(())
}

fn restart(args: Args) -> Result<(), String> {
    let context = Context::open(&args)?;
    let group = context.scope()?.group(GROUP);
    let subscription = context
        .engine
        .observe(group.read(kinds([9])).map_err(display)?, None)
        .map_err(display)?;
    let mut observed = Observed::default();
    wait_until(&subscription, context.settle, &mut observed, |state| {
        state.rows_of_kind(9).len() >= 27
            && state.has_source_count(SHARED_CHAT, 2)
            && state.relays_in_evidence().len() == 2
    })?;
    ensure(
        observed
            .evidence
            .iter()
            .flat_map(|branch| branch.sources.iter())
            .all(|source| source.reconciled_through.is_some()),
        "offline restart lost the persisted per-source coverage watermark",
    )?;
    signal_ready(&args)?;
    println!(
        "PROOF restart_offline cached_rows={} shared_sources=2 persisted_watermarks=true statuses={:?}",
        observed.rows_of_kind(9).len(),
        observed
            .evidence
            .iter()
            .flat_map(|branch| branch.sources.iter().map(|source| source.status))
            .collect::<Vec<_>>()
    );

    wait_until(&subscription, context.settle, &mut observed, |state| {
        let sources: Vec<_> = state
            .evidence
            .iter()
            .flat_map(|branch| branch.sources.iter())
            .collect();
        // Either connected-and-live state proves the reconnect resubscribed
        // (#1235). `FinishedStoredEvents` proves strictly more -- it asked
        // AND was answered -- so demanding `Requesting` alone would wait for
        // the engine to be SLOWER than it is.
        sources.len() >= 2
            && sources.iter().all(|source| {
                matches!(
                    source.status,
                    SourceStatus::Requesting | SourceStatus::FinishedStoredEvents
                )
            })
    })?;
    println!(
        "PROOF restart_reconnected relays=2 statuses={:?}",
        observed
            .evidence
            .iter()
            .flat_map(|branch| branch.sources.iter().map(|source| source.status))
            .collect::<Vec<_>>()
    );
    println!("PASS restart");
    drop(subscription);
    context.shutdown();
    Ok(())
}

fn restart_conflict(args: Args) -> Result<(), String> {
    let context = Context::open(&args)?;
    let scope = context.scope()?;
    let watching = metadata_observation(&context, &scope)?;

    // Relay B is offline, and its own record survived the restart in the
    // local store. The app must still SEE what relay B signed -- absence of a
    // link is not absence of a record -- while being told, per host, that
    // relay B is not currently proven.
    let offline = wait_for_snapshots(&context, &watching, "offline restart", |snapshots| {
        bitcoin(snapshots).is_some_and(|snapshot| {
            metadata_names(snapshot).is_ok_and(|names| {
                names.get(&context.relay_a).map(String::as_str) == Some("Bitcoin Cash live")
                    && names.get(&context.relay_b).map(String::as_str)
                        == Some("Bitcoin (real) live")
            }) && host_availability(snapshot, &context.relay_a) == Some(GroupAvailability::Ready)
                && host_availability(snapshot, &context.relay_b) != Some(GroupAvailability::Ready)
        })
    })?;
    let offline = bitcoin(&offline).expect("the predicate proved it is there");
    ensure(
        offline.availability != GroupAvailability::Ready,
        "one host is unproven, so the hoisted availability must not read Ready",
    )?;
    signal_ready(&args)?;
    println!(
        "PROOF restart_conflict_offline metadata={:?} cached_sources=2 hoisted={:?} per_host={:?}",
        metadata_names(offline)?,
        offline.availability,
        host_availabilities(offline)
    );

    let reconnected = wait_for_snapshots(&context, &watching, "reconnected", |snapshots| {
        bitcoin(snapshots).is_some_and(|snapshot| {
            host_availability(snapshot, &context.relay_b) == Some(GroupAvailability::Ready)
        })
    })?;
    let reconnected = bitcoin(&reconnected).expect("the predicate proved it is there");
    println!(
        "PROOF restart_conflict_reconnected metadata={:?} per_host={:?}",
        metadata_names(reconnected)?,
        host_availabilities(reconnected)
    );
    println!("PASS restart-conflict");
    drop(watching);
    context.shutdown();
    Ok(())
}

fn host_availability(
    snapshot: &nip29::GroupSnapshot,
    host: &RelayUrl,
) -> Option<GroupAvailability> {
    snapshot.at(host).map(|records| records.availability)
}

fn host_availabilities(snapshot: &nip29::GroupSnapshot) -> BTreeMap<RelayUrl, GroupAvailability> {
    snapshot
        .per_host
        .iter()
        .map(|(host, records)| (host.clone(), records.availability))
        .collect()
}

fn wait_for_write(
    receipts: &nmp::FifoReceiver<WriteFact>,
    timeout: Duration,
    predicate: impl Fn(&[WriteFact]) -> bool,
) -> Result<Vec<WriteFact>, String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut statuses = Vec::new();
    while !predicate(&statuses) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(format!("receipt timed out: {statuses:?}"));
        }
        statuses.push(
            receipts
                .recv_timeout(remaining)
                .map_err(|error| format!("receipt ended early: {error:?}; saw {statuses:?}"))?,
        );
    }
    Ok(statuses)
}

fn acked(statuses: &[WriteFact], relay: &RelayUrl) -> bool {
    statuses
        .iter()
        .any(|status| matches!(status, WriteFact::Relay { relay: candidate, state: RelayState::Published } if candidate == relay))
}

fn signal_ready(args: &Args) -> Result<(), String> {
    if let Some(path) = &args.ready_file {
        fs::write(path, b"ready\n")
            .map_err(|error| format!("could not write ready file {}: {error}", path.display()))?;
    }
    Ok(())
}

fn kinds<const N: usize>(values: [u16; N]) -> Filter {
    Filter {
        kinds: Some(BTreeSet::from(values)),
        ..Filter::default()
    }
}

fn parse_relay(label: &str, value: &str) -> Result<RelayUrl, String> {
    RelayUrl::parse(value).map_err(|error| format!("invalid {label} URL: {error}"))
}

fn ensure(condition: bool, error: impl Into<String>) -> Result<(), String> {
    condition.then_some(()).ok_or_else(|| error.into())
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod secret_key_tests {
    use super::decode_hex_secret;

    #[test]
    fn decodes_exact_secret_bytes_at_the_application_boundary() {
        let encoded = b"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n";
        assert_eq!(
            decode_hex_secret(encoded).unwrap(),
            [
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
                0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
                0x1c, 0x1d, 0x1e, 0x1f,
            ]
        );
    }

    #[test]
    fn refuses_encoded_or_malformed_secret_values_below_the_boundary() {
        assert!(decode_hex_secret(b"nsec1not-a-decoded-key").is_err());
        assert!(decode_hex_secret(
            b"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1g"
        )
        .is_err());
    }
}
