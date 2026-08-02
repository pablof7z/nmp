use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::num::NonZeroUsize;
use std::time::Duration;

use nmp::{
    nip29, AccessContext, Binding, CacheMode, Demand, Derived, Engine, EngineConfig, EventBuilder,
    Filter, IdentityField, Kind, PublicKey, RelayUrl, Selector, SourceAuthority, SourceStatus, Tag,
    Window, WriteStatus,
};

use crate::args::{Args, Mode};
use crate::observe::{tag_value, wait_until, Observed};

const GROUP: &str = "bitcoin";
const SINGLE_GROUP: &str = "solo-a";
const MIXED_GROUP: &str = "one-sided";
const SHARED_CHAT: &str = "shared chat observed at both hosts";
const RELAY_B_CHAT: &str = "relay B chat";

pub fn run(args: Args) -> Result<(), String> {
    match args.mode {
        Mode::Online => online(args),
        Mode::ProvenanceGrowth => provenance_growth(args),
        Mode::Restart => restart(args),
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
    _writer_registration: nmp::AccountRegistration,
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
            allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
            max_relays: 4,
            ..EngineConfig::default()
        })
        .map_err(|error| format!("engine construction failed: {error}"))?;
        engine
            .set_active_account(Some(viewer))
            .map_err(|error| format!("active viewer installation failed: {error}"))?;

        let secret = fs::read_to_string(&args.writer_secret_file)
            .map_err(|error| format!("could not read writer secret file: {error}"))?;
        let registration = engine
            .add_account(secret.trim())
            .map_err(|error| format!("writer registration failed: {error}"))?;
        let writer = registration.public_key();

        Ok(Self {
            engine,
            relay_a,
            relay_b,
            viewer,
            followed,
            outsider,
            writer,
            _writer_registration: registration,
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
    // Relay-signed discovery records are keyed by `d`, unlike group content's
    // `h`. Positive member-list evidence at each relay selects both records.
    let subjects = Binding::Literal(BTreeSet::from([
        context.followed.to_hex(),
        context.outsider.to_hex(),
    ]));
    let query = scope
        .groups_where(&nip29::member_list_includes(subjects))
        .map_err(display)?;
    let subscription = context.engine.observe(query, None).map_err(display)?;
    let mut observed = Observed::default();
    wait_until(&subscription, context.settle, &mut observed, |state| {
        state.rows_of_kind(39000).len() == 2
    })?;
    let names_by_source: BTreeMap<_, _> = observed
        .rows_of_kind(39000)
        .into_iter()
        .map(|row| {
            let source = row
                .sources
                .iter()
                .next()
                .cloned()
                .ok_or_else(|| "relay metadata row had no source".to_string())?;
            let name = tag_value(row, "name")
                .ok_or_else(|| "relay metadata row had no name".to_string())?;
            Ok((source, name))
        })
        .collect::<Result<_, String>>()?;
    ensure(
        names_by_source.get(&context.relay_a).map(String::as_str) == Some("Bitcoin Cash")
            && names_by_source.get(&context.relay_b).map(String::as_str) == Some("Bitcoin (real)"),
        format!("conflicting relay metadata was not preserved: {names_by_source:?}"),
    )?;
    let display_winner = names_by_source
        .get(&context.relay_b)
        .expect("relay B's asserted row exists");
    println!(
        "PROOF metadata_conflict preserved=2 app_winner={display_winner:?} policy=prefer_relay_b"
    );
    Ok(())
}

fn verify_follows_discovery(context: &Context, scope: &nip29::RelayScope) -> Result<(), String> {
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
    let predicate = nip29::member_list_includes(follows);
    let query = scope.groups_where(&predicate).map_err(display)?;
    let subscription = context.engine.observe(query, None).map_err(display)?;
    let mut observed = Observed::default();
    wait_until(&subscription, context.settle, &mut observed, |state| {
        state.rows.values().any(|row| {
            row.event.kind.as_u16() == 39002
                && tag_value(row, "d").as_deref() == Some(GROUP)
                && row.event.tags.iter().any(|tag| {
                    let values = tag.as_slice();
                    values.first().map(String::as_str) == Some("p")
                        && values.get(1).map(String::as_str)
                            == Some(context.followed.to_hex().as_str())
                })
        })
    })?;
    ensure(
        observed.rows.values().all(|row| {
            tag_value(row, "d").as_deref() == Some(GROUP)
                && row.sources == BTreeSet::from([context.relay_a.clone()])
        }),
        "follows-derived discovery crossed relay authority or returned another group",
    )?;
    println!(
        "PROOF discovery predicate=member_list_includes(follows_of_active_viewer) group={GROUP} viewer={} followed={} evidence_source={}",
        context.viewer, context.followed, context.relay_a
    );
    Ok(())
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
    let chat_statuses = wait_for_write(&chat, context.settle, |statuses| {
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
    let article_statuses = wait_for_write(&article, context.settle, |statuses| {
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
    let mixed_statuses = wait_for_write(&mixed, context.settle, |statuses| {
        acked(statuses, &context.relay_a)
            && statuses.iter().any(
                |status| matches!(status, WriteStatus::Rejected(relay, _) if relay == &context.relay_b),
            )
    })?;
    println!("PROOF publish mixed_group={MIXED_GROUP} outcomes={mixed_statuses:?}");
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
                .any(|row| row.event.content == "relay A chat")
    })?;
    ensure(
        !observed
            .rows
            .values()
            .any(|row| row.event.content == RELAY_B_CHAT),
        "relay B-only content arrived while relay B was staged down",
    )?;
    signal_ready(&args)?;
    println!("PROOF provenance_before shared_sources=1 relay_b_content=false");

    wait_until(&subscription, context.settle, &mut observed, |state| {
        state.has_source_count(SHARED_CHAT, 2)
            && state
                .rows
                .values()
                .any(|row| row.event.content == RELAY_B_CHAT)
    })?;
    let shared = observed
        .rows
        .values()
        .find(|row| row.event.content == SHARED_CHAT)
        .expect("shared row exists after predicate");
    ensure(
        observed.source_growth.contains(&shared.event.id),
        "the existing shared row reached two sources without SourcesGrew",
    )?;
    println!(
        "PROOF provenance_after shared_sources=2 relay_b_content=true sources_grew_event={}",
        shared.event.id
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
        sources.len() >= 2
            && sources
                .iter()
                .all(|source| source.status == SourceStatus::Requesting)
    })?;
    println!("PROOF restart_reconnected relays=2 statuses=Requesting");
    println!("PASS restart");
    drop(subscription);
    context.shutdown();
    Ok(())
}

fn wait_for_write(
    receipts: &nmp::FifoReceiver<WriteStatus>,
    timeout: Duration,
    predicate: impl Fn(&[WriteStatus]) -> bool,
) -> Result<Vec<WriteStatus>, String> {
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

fn acked(statuses: &[WriteStatus], relay: &RelayUrl) -> bool {
    statuses
        .iter()
        .any(|status| matches!(status, WriteStatus::Acked(candidate) if candidate == relay))
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
