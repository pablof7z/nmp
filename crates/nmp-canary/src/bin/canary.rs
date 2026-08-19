//! Drives `nmp-canary` against a real `nmp::Engine` with a local store and no
//! reachable relay. This is the proof that the library above is not merely
//! type-correct: every surface is opened, written to, and read back.
//!
//! No relay means every write settles as `NoDestination` and every read is
//! served from the canonical local store. That is enough to exercise the
//! SURFACE, which is what this crate exists to measure. The relay half is a
//! separate harness.
//!
//! It is a BINARY and not a test, and that is load-bearing. Five of the
//! scenarios below cannot be expressed any other way:
//!
//! - a restart is only a restart if the writing process exited. A second
//!   `Engine` over one store in one address space still holds the redb pages,
//!   the allocator and every decoded row, so a read served from anywhere but
//!   the durable file looks identical to a correct one.
//! - a crash is only a crash if the process was SIGKILLed mid-flight, with no
//!   `shutdown` and no `Drop`.
//! - descriptors, threads and resident size are properties of a process.
//! - "the process exited" and "teardown returned" are different signals.
//! - two processes contending for one store is not a function call.
//!
//! Run: `cargo run -p nmp-canary --bin canary [scenario]`
//!
//! Scenarios: `all` (default), `surfaces`, `deletions`, `routing`, `authgate`,
//! `restart`, `crash`, `contend`, `teardown`, `findings`. The `child-*` forms are spawned
//! by the supervisors and are not meant to be typed.

use std::time::Duration;

use nmp::{PublicKey, RelayUrl};
use nmp_canary::composer::Composer;
use nmp_canary::feed::FollowsFeed;
use nmp_canary::people::{Follows, Mutes};
use nmp_canary::profiles::ProfileBook;
use nmp_canary::room::Room;
use nmp_canary::rows::{RowTable, RowView};
use nmp_canary::thread::ThreadView;
use nmp_canary::Canary;

const TICK: Duration = Duration::from_millis(250);

fn surfaces() {
    let mut app = Canary::open(None, Vec::new()).expect("engine opens with no relays");

    // --- two accounts live at once ------------------------------------------
    let alice = app
        .add_account(&[7u8; 32], true)
        .expect("alice's key is valid");
    let bob = app
        .add_account(&[9u8; 32], false)
        .expect("bob's key is valid");
    println!("alice   {}", short(alice));
    println!("bob     {}", short(bob));
    println!("current {:?}", app.current().map(short));

    // --- follow, through the capability that owns it -------------------------
    let follows = Follows::new();
    let mut receipt = follows
        .follow(&app.engine, bob)
        .expect("alice can follow bob");
    let facts = drain(&mut receipt);
    println!("\nfollow receipt: {facts} fact(s)");

    // --- the follows feed ----------------------------------------------------
    let mut feed = FollowsFeed::open(&app.engine, 20, 200).expect("feed opens");
    pump(&mut feed, 6);
    println!(
        "feed: {} row(s), load {:?}, empty-state {:?}",
        feed.rows().len(),
        feed.load(),
        feed.empty_state()
    );
    for (relay, status) in feed.per_relay() {
        println!("  source {relay} {status:?}");
    }

    // --- compose: bob posts, alice's feed should show it ---------------------
    let mut post =
        Composer::post(&app.engine, bob, "first light from the canary").expect("bob can post");
    post.drain(&app.engine, TICK);
    let post_id = post
        .state
        .event_id
        .expect("acceptance decided the event id");
    println!(
        "\npost: event {} receipt {:?} signing {:?} outcome {:?}",
        &post_id.to_hex()[..12],
        post.state.receipt,
        post.state.signing,
        post.state.outcome
    );
    println!(
        "  published fraction {:?}, route complete {}, awaiting routes for {} author(s)",
        post.state.published_fraction(),
        post.state.route_complete,
        post.state.awaiting_routes.len()
    );

    pump(&mut feed, 6);
    println!("feed after post: {} row(s)", feed.rows().len());
    let mut people = ProfileBook::new();
    for row in feed.rows() {
        let view = RowView::of(row, Some(people.label(row.pubkey())), None);
        println!(
            "  row {} by {} signed={} blocks={} room={:?} sources={}",
            &view.id.to_hex()[..12],
            view.author_display.as_deref().unwrap_or("?"),
            view.signed,
            view.blocks,
            view.room,
            view.sources.len()
        );
    }

    // --- profiles ------------------------------------------------------------
    publish_profile(&app, bob, "bob", "https://example.invalid/bob.png");
    let profile_query = nmp_canary::profiles::profiles_of_authors([alice, bob]);
    let profiles = app
        .engine
        .observe(profile_query, None)
        .expect("profile observation opens");
    for _ in 0..6 {
        match profiles.recv_timeout(TICK) {
            Ok(frame) => people.apply(&frame),
            Err(_) => break,
        }
    }
    println!("\nprofiles known: {}", people.len());
    println!("  bob renders as {:?}", people.label(bob));
    println!("  alice renders as {:?}", people.label(alice));

    // --- thread --------------------------------------------------------------
    let mut thread = ThreadView::open(&app.engine, post_id).expect("thread opens");
    for _ in 0..4 {
        if thread.next_within(TICK).is_none() {
            break;
        }
    }
    let parent_row = thread
        .table()
        .get(&post_id)
        .cloned()
        .expect("the root is in the thread");
    let mut reply =
        Composer::reply(&app.engine, alice, &parent_row, "and a reply").expect("alice can reply");
    reply.drain(&app.engine, TICK);
    for _ in 0..6 {
        if thread.next_within(TICK).is_none() {
            break;
        }
    }
    println!("\nthread rows: {}", thread.table().len());
    for (depth, row) in thread.rendered() {
        println!(
            "  depth {depth} {} {:?}",
            &row.id().to_hex()[..12],
            row.content().chars().take(30).collect::<String>()
        );
    }

    // --- react / repost, and the pending-row wall ----------------------------
    match Composer::react(&app.engine, alice, &parent_row, "\u{1f426}") {
        Ok(mut sending) => {
            sending.drain(&app.engine, TICK);
            println!(
                "\nreaction accepted: {:?}",
                sending.state.event_id.is_some()
            );
        }
        Err(error) => println!("\nreaction refused: {error}"),
    }
    match Composer::repost(&app.engine, alice, &parent_row) {
        Ok(mut sending) => {
            sending.drain(&app.engine, TICK);
            println!("repost accepted: {:?}", sending.state.event_id.is_some());
        }
        Err(error) => println!("repost refused: {error}"),
    }

    // --- the send indicator on an already-rendered row ------------------------
    match Composer::delivery_of(&app.engine, post_id) {
        Some(state) => println!(
            "\ndelivery_of(post): intended {} relays, complete {}, outcome {:?}",
            state.intended.len(),
            state.route_complete,
            state.outcome
        ),
        None => println!("\ndelivery_of(post): no retained receipt"),
    }

    // --- mute, by hand --------------------------------------------------------
    let mute_view = app
        .engine
        .observe(Mutes::my_mute_list(), None)
        .expect("mute observation opens");
    let mut mute_table = RowTable::new();
    for _ in 0..3 {
        match mute_view.recv_timeout(TICK) {
            Ok(frame) => mute_table.apply(&frame),
            Err(_) => break,
        }
    }
    let current_list = mute_table.rows().next().cloned();
    let mut muting =
        Mutes::mute(&app.engine, alice, current_list.as_ref(), bob).expect("a mute list publishes");
    let facts = drain(&mut muting);
    println!("\nmute receipt: {facts} fact(s)");
    for _ in 0..4 {
        match mute_view.recv_timeout(TICK) {
            Ok(frame) => mute_table.apply(&frame),
            Err(_) => break,
        }
    }
    let muted = mute_table
        .rows()
        .next()
        .map(|row| Mutes::muted(row, bob))
        .unwrap_or(false);
    println!("bob muted: {muted}");
    pump(&mut feed, 6);
    println!("feed after mute: {} row(s)", feed.rows().len());

    // --- the follow button ----------------------------------------------------
    let button = nmp_canary::people::FollowButton::open(app.engine.clone(), bob)
        .expect("the follow observation opens");
    match button.wait(Duration::from_secs(2)) {
        Some(snapshot) => println!(
            "\nfollow button: {:?} / {:?} (base {:?})",
            snapshot.relationship,
            snapshot.availability,
            snapshot
                .base_event_id
                .map(|id| id.to_hex()[..12].to_string())
        ),
        None => println!("\nfollow button: no snapshot inside 2s"),
    }
    drop(button);

    // --- notifications --------------------------------------------------------
    let notifications = app
        .engine
        .observe(
            nmp_canary::notifications::mentions_of(bob, [1u16, 6, 7]),
            None,
        )
        .expect("notification observation opens");
    let mut inbox = RowTable::new();
    for _ in 0..6 {
        match notifications.recv_timeout(TICK) {
            Ok(frame) => inbox.apply(&frame),
            Err(_) => break,
        }
    }
    println!("\nnotifications for bob: {}", inbox.len());
    for row in inbox.rows() {
        println!(
            "  {:?} from {}",
            nmp_canary::notifications::classify(row),
            short(row.pubkey())
        );
    }

    // --- a room ---------------------------------------------------------------
    // A host that is not there. The write parks and the read serves the local
    // canonical store, which is exactly what a room screen does on a plane.
    let host = RelayUrl::parse("wss://127.0.0.1:1").expect("a well-formed relay url");
    let mut room = Room::open(&app.engine, [host.clone()], "canary-room", [9u16, 11, 12])
        .expect("the room opens");
    let mut posting = room
        .post(&app.engine, alice, 9, "hello from inside the room")
        .expect("alice can post into the room");
    let facts = drain(&mut posting);
    println!("\nroom post receipt: {facts} fact(s)");
    for _ in 0..8 {
        if room.poll_timeline(TICK).is_none() {
            break;
        }
    }
    println!("room timeline: {} row(s)", room.table().len());
    for row in room.table().rows() {
        let view = RowView::of(row, None, None);
        println!(
            "  {} room={:?} signed={} {:?}",
            &view.id.to_hex()[..12],
            view.room,
            view.signed,
            row.content().chars().take(30).collect::<String>()
        );
    }

    // The async half of the same screen, on an executor the app had to bring.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("the app builds its own executor because nmp does not lend one");
    let snapshots = runtime.block_on(async {
        tokio::time::timeout(Duration::from_millis(500), room.next_records())
            .await
            .ok()
            .flatten()
    });
    println!(
        "room records delivery: {:?} snapshot(s), latest {:?}",
        snapshots.as_ref().map(Vec::len),
        room.snapshot().map(|snapshot| snapshot.availability)
    );
    println!(
        "  alice is a member: {:?}, an admin: {:?}",
        room.is_member(alice),
        room.is_admin(alice)
    );

    // --- rooms list -----------------------------------------------------------
    match nmp_canary::room::rooms_list(&app.engine, [host]) {
        Ok(list) => println!("\nrooms list: {} known", list.latest().len()),
        Err(error) => println!("\nrooms list failed: {error}"),
    }

    // --- diagnostics ----------------------------------------------------------
    let diagnostics = app.diagnostics().expect("diagnostics open");
    match diagnostics.recv() {
        Some(snapshot) => println!(
            "\ndiagnostics: {} relay row(s), {} stalled write(s), uncovered authors {}",
            snapshot.relays.len(),
            snapshot.stalled_writes.len(),
            snapshot.uncovered_author_count
        ),
        None => println!("\ndiagnostics: stream closed"),
    }

    drop(feed);
    drop(room);
    app.shutdown();
}

fn short(key: PublicKey) -> String {
    key.to_hex()[..12].to_string()
}

fn pump(feed: &mut FollowsFeed, times: usize) {
    for _ in 0..times {
        if feed.next_within(TICK).is_none() {
            break;
        }
    }
}

/// Drain a receipt to its terminal, counting facts. Uses the SUPPORTED door
/// (`ReceiptStream::result`) rather than the app-side fold in
/// `composer::Sending::drain`, so both paths are exercised.
fn drain(receipt: &mut nmp::ReceiptStream) -> usize {
    let mut seen = 0usize;
    loop {
        match receipt.statuses.recv_timeout(TICK) {
            Ok(fact) => {
                seen += 1;
                if matches!(fact, nmp::WriteFact::Outcome(_)) {
                    return seen;
                }
            }
            Err(_) => return seen,
        }
    }
}

fn publish_profile(app: &Canary, author: PublicKey, name: &str, picture: &str) {
    let body = serde_json::json!({ "name": name, "picture": picture }).to_string();
    let intent = nmp::WriteIntent {
        payload: nmp::WritePayload::Event(
            nmp::EventBuilder::new(nmp::Kind::from(nmp_canary::profiles::METADATA_KIND))
                .content(body),
        ),
        routing: nmp::WriteRouting::Auto,
        identity: nmp::Identity::Explicit(author),
    };
    if let Ok(mut receipt) = app.engine.publish(intent) {
        drain(&mut receipt);
    }
}

// ===========================================================================
// Dispatcher
// ===========================================================================

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scenario = args.get(1).map(String::as_str).unwrap_or("all");
    match scenario {
        "surfaces" => surfaces(),
        "deletions" => deletions_scenario(),
        "routing" => routing_scenario(),
        "authgate" => authgate_scenario(),
        "restart" => restart_scenario(),
        "crash" => crash_scenario(),
        "contend" => contend_scenario(),
        "teardown" => teardown_scenario(),
        "findings" => print!("{}", nmp_canary::report()),
        // Spawned by the supervisors above. Not meant to be typed.
        "child-write" => child_write(&args),
        "child-recover" => child_recover(&args),
        "child-contend" => child_contend(&args),
        "all" => {
            surfaces();
            deletions_scenario();
            routing_scenario();
            authgate_scenario();
            restart_scenario();
            crash_scenario();
            contend_scenario();
            teardown_scenario();
            println!("\n{}", "=".repeat(72));
            print!("{}", nmp_canary::report());
        }
        other => {
            eprintln!("unknown scenario {other:?}");
            std::process::exit(2);
        }
    }
}

fn banner(title: &str) {
    println!("\n{}\n== {title}\n{}", "=".repeat(72), "-".repeat(72));
}

/// This binary's own path, for spawning children.
fn me() -> std::path::PathBuf {
    std::env::current_exe().expect("a running binary knows its own path")
}

fn scratch(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("nmp-canary-{}-{}", name, std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir.join("store.redb").to_string_lossy().into_owned()
}

// ===========================================================================
// NIP-09 deletions: the kind an app must ask for and then hide
// ===========================================================================

fn deletions_scenario() {
    banner("NIP-09 deletions");
    let mut app = Canary::open(None, Vec::new()).expect("engine opens");
    let alice = app.add_account(&[7u8; 32], true).expect("alice");
    let bob = app.add_account(&[9u8; 32], false).expect("bob");
    let follows = Follows::new();
    let mut following = follows.follow(&app.engine, bob).expect("alice follows bob");
    drain(&mut following);

    let mut post = Composer::post(&app.engine, bob, "this one gets deleted").expect("bob posts");
    post.drain(&app.engine, TICK);
    let doomed = post.state.event_id.expect("acceptance decided the id");

    let mut feed = FollowsFeed::open(&app.engine, 20, 200).expect("feed opens");
    // The control: identical query with kind:5 left out.
    let mut control = FollowsFeed::open_with(
        &app.engine,
        nmp_canary::feed::follows_feed_without_deletions(),
        20,
        200,
    )
    .expect("the control feed opens");
    pump(&mut feed, 6);
    pump(&mut control, 6);
    println!(
        "before delete: subscribed-5 feed {} raw / {} displayed; control (no kind:5) {} raw",
        feed.rows().len(),
        feed.display_rows().count(),
        control.rows().len()
    );

    // The deletion itself. No NIP-09 crate owns this, so the app composes it.
    let target = feed
        .rows()
        .iter()
        .find(|row| row.id() == doomed)
        .cloned()
        .expect("the doomed row is in the feed");
    let mut deleting = nmp_canary::deletions::delete(&app.engine, bob, &target, "on reflection")
        .expect("a deletion publishes like any other write");
    drain(&mut deleting);
    pump(&mut feed, 8);
    pump(&mut control, 8);

    let raw = feed.rows().len();
    let shown = feed.display_rows().count();
    let deletions_in_set = feed
        .rows()
        .iter()
        .filter(|row| nmp_canary::deletions::is_deletion(row))
        .count();
    let doomed_still_there = feed.rows().iter().any(|row| row.id() == doomed);
    println!(
        "after delete:  {raw} raw row(s), {shown} displayed, {deletions_in_set} of them kind:5"
    );
    println!("  the deleted row is still in the row set: {doomed_still_there}");
    println!(
        "  control feed (kind:5 NOT subscribed): {} raw row(s), deleted row present: {}",
        control.rows().len(),
        control.rows().iter().any(|row| row.id() == doomed)
    );
    println!();
    println!("  MEASURED here: a subscribed kind:5 becomes a row the timeline must filter out,");
    println!("  and a LOCAL deletion tombstones its target whether or not kind:5 was subscribed");
    println!("  (nmp-store applies it inside `insert`, independent of any open query).");
    println!("  NOT measured here (no relay in this harness): that a REMOTE deletion never");
    println!("  arrives unless the app widened its own kinds. That half rests on the source --");
    println!("  nothing in nmp-router/nmp-resolver/nmp-engine widens a demand's `kinds` to 5.");
    let _ = alice;
    drop(feed);
    drop(control);
    app.shutdown();
}

// ===========================================================================
// WriteRouting::Auto with nothing configured
// ===========================================================================

fn routing_scenario() {
    banner("WriteRouting::Auto under four configurations");
    println!("claim under test: 'Auto is REFUSED unless outbox indexers are configured'\n");
    for probe in nmp_canary::routing::matrix() {
        match nmp_canary::routing::run(&probe, Duration::from_millis(700)) {
            Ok(observed) => println!("  {observed}"),
            Err(error) => println!("  {:<28} engine would not start: {error}", probe.label),
        }
    }
}

// ===========================================================================
// Reading from a relay that gates reads behind NIP-42
// ===========================================================================

fn authgate_scenario() {
    banner("a read against a relay that gates reads behind NIP-42");
    println!(
        "claim under test: 'pinning authenticate_as + a registered signer + an\n\
         allowing AuthPolicy is enough to read from an auth-gated relay'\n"
    );
    for case in nmp_canary::authgate::matrix() {
        match nmp_canary::authgate::run(&case, Duration::from_secs(8)) {
            Ok(observed) => println!("{observed}"),
            Err(error) => println!("  {:<28} engine would not start: {error}", case.label),
        }
    }
}

// ===========================================================================
// A real restart: the writing process exits before anything is read back
// ===========================================================================

fn restart_scenario() {
    banner("restart across a real process exit");
    let store = scratch("restart");
    let _ = nmp::Engine::reset_persistent_store(&store);

    let write = std::process::Command::new(me())
        .args(["child-write", &store, "3", "clean"])
        .output()
        .expect("the child runs");
    let stdout = String::from_utf8_lossy(&write.stdout).into_owned();
    let handoff = nmp_canary::process::Handoff::parse(&stdout);
    println!(
        "writer pid exited with {:?}, handed off {} event id(s)",
        write.status.code(),
        handoff.events.len()
    );
    for line in stdout.lines().filter(|line| line.starts_with("child.")) {
        println!("  {line}");
    }
    if !write.status.success() {
        println!(
            "  writer stderr: {}",
            String::from_utf8_lossy(&write.stderr)
        );
    }

    let mut recover_args = vec!["child-recover".to_string(), store.clone()];
    recover_args.extend(handoff.receipts.iter().map(|id| id.0.to_string()));
    let recover = std::process::Command::new(me())
        .args(&recover_args)
        .output()
        .expect("the recovering child runs");
    println!("reader pid exited with {:?}", recover.status.code());
    for line in String::from_utf8_lossy(&recover.stdout)
        .lines()
        .filter(|line| line.starts_with("child."))
    {
        println!("  {line}");
    }
    if !recover.status.success() {
        println!(
            "  reader stderr: {}",
            String::from_utf8_lossy(&recover.stderr)
        );
    }
    let _ = nmp::Engine::reset_persistent_store(&store);
}

// ===========================================================================
// SIGKILL mid-publish, recovered through the durable doors only
// ===========================================================================

fn crash_scenario() {
    banner("SIGKILL of a live engine with writes in flight");
    let store = scratch("crash");
    let _ = nmp::Engine::reset_persistent_store(&store);

    let mut child = std::process::Command::new(me())
        .args(["child-write", &store, "4", "hang"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the child spawns");

    // Wait for the child to say it has accepted its writes, then kill it
    // without giving it any chance to run `shutdown` or `Drop`.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut buffer = String::new();
        let mut chunk = [0u8; 512];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    buffer.push_str(&String::from_utf8_lossy(&chunk[..read]));
                    if buffer.contains("child.ready") {
                        let _ = tx.send(buffer.clone());
                        break;
                    }
                }
            }
        }
        let _ = tx.send(buffer);
    });

    let observed = rx
        .recv_timeout(Duration::from_secs(20))
        .unwrap_or_else(|_| String::new());
    let handoff = nmp_canary::process::Handoff::parse(&observed);
    println!(
        "child accepted {} write(s) and reported ready; killing it now",
        handoff.events.len()
    );
    for line in observed.lines().filter(|line| line.starts_with("child.")) {
        println!("  {line}");
    }
    // SIGKILL. No unwinding, no flush, no `Drop`.
    let _ = child.kill();
    let status = child.wait().expect("the killed child is reaped");
    println!("child terminated: {status:?}");

    let mut recover_args = vec!["child-recover".to_string(), store.clone()];
    recover_args.extend(handoff.receipts.iter().map(|id| id.0.to_string()));
    let recover = std::process::Command::new(me())
        .args(&recover_args)
        .output()
        .expect("the recovering child runs");
    println!("recovery pid exited with {:?}", recover.status.code());
    for line in String::from_utf8_lossy(&recover.stdout)
        .lines()
        .filter(|line| line.starts_with("child."))
    {
        println!("  {line}");
    }
    if !recover.status.success() {
        println!(
            "  recovery stderr: {}",
            String::from_utf8_lossy(&recover.stderr)
        );
    }
    let _ = nmp::Engine::reset_persistent_store(&store);
}

// ===========================================================================
// Two processes, one store
// ===========================================================================

fn contend_scenario() {
    banner("two processes over one store");
    let store = scratch("contend");
    let _ = nmp::Engine::reset_persistent_store(&store);
    let app = nmp_canary::process::durable(&store).expect("this process opens the store");
    println!("this process holds the store open");

    let child = std::process::Command::new(me())
        .args(["child-contend", &store])
        .output()
        .expect("the contending child runs");
    for line in String::from_utf8_lossy(&child.stdout)
        .lines()
        .filter(|line| line.starts_with("child."))
    {
        println!("  {line}");
    }

    // And the documented in-process refusal, for contrast.
    match nmp::Engine::reset_persistent_store(&store) {
        Ok(()) => println!("  reset while open: ACCEPTED (no in-process refusal)"),
        Err(error) => println!("  reset while open: refused -- {error}"),
    }
    app.shutdown();
    let _ = nmp::Engine::reset_persistent_store(&store);
}

// ===========================================================================
// Teardown returned, and the process exits, with work in flight
// ===========================================================================

fn teardown_scenario() {
    banner("teardown with work in flight");
    let before = nmp_canary::process::Survey::take();
    println!("before: {before}");

    let mut app = Canary::open(None, Vec::new()).expect("engine opens");
    let alice = app.add_account(&[11u8; 32], true).expect("alice");

    // Work in flight: parked writes and open observations, none of them settled.
    let mut held = Vec::new();
    for index in 0..8 {
        if let Ok(sending) = Composer::post(&app.engine, alice, &format!("in flight {index}")) {
            held.push(sending);
        }
    }
    let feeds: Vec<_> = (0..4)
        .filter_map(|_| FollowsFeed::open(&app.engine, 10, 50).ok())
        .collect();
    let during = nmp_canary::process::Survey::take();
    println!(
        "during: {during} (writes held {}, observations open {})",
        held.len(),
        feeds.len()
    );

    let elapsed = nmp_canary::process::timed_shutdown(&app.engine);
    println!("shutdown RETURNED after {elapsed:?}");
    println!("  (`shutdown` returns `()`, so its return is the whole signal --");
    println!("   there is no way to ask whether anything was abandoned)");

    drop(feeds);
    drop(held);
    let after = nmp_canary::process::Survey::take();
    println!("after:  {after}");
    println!(
        "  descriptor delta across the whole scenario: {:?}",
        after.delta_descriptors(&before)
    );
}

// ===========================================================================
// Children
// ===========================================================================

fn child_write(args: &[String]) {
    let store = args.get(2).cloned().unwrap_or_default();
    let count: usize = args
        .get(3)
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let mode = args.get(4).map(String::as_str).unwrap_or("clean");

    let mut app = match nmp_canary::process::durable(&store) {
        Ok(app) => app,
        Err(error) => {
            println!("child.open=failed:{error}");
            std::process::exit(1);
        }
    };
    let author = app.add_account(&[13u8; 32], true).expect("a valid key");
    println!("child.pid={}", std::process::id());
    println!("child.survey={}", nmp_canary::process::Survey::take());

    let mut handoff = nmp_canary::process::Handoff {
        author: Some(author),
        events: Vec::new(),
        receipts: Vec::new(),
    };
    for index in 0..count {
        match Composer::post(&app.engine, author, &format!("durable write {index}")) {
            Ok(mut sending) => {
                sending.drain(&app.engine, Duration::from_millis(200));
                if let Some(event) = sending.state.event_id {
                    handoff.events.push(event);
                }
                if let Some(receipt) = sending.state.receipt {
                    handoff.receipts.push(receipt);
                }
            }
            Err(error) => println!("child.publish_failed={error}"),
        }
    }
    handoff.print();
    println!("child.accepted={}", handoff.events.len());
    println!("child.ready=1");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    if mode == "hang" {
        // Wait to be killed. No shutdown, no Drop, no flush beyond what the
        // engine already committed at acceptance.
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    let elapsed = nmp_canary::process::timed_shutdown(&app.engine);
    println!("child.shutdown_returned_in={elapsed:?}");
    let _ = std::io::stdout().flush();
}

fn child_recover(args: &[String]) {
    let store = args.get(2).cloned().unwrap_or_default();
    // Receipt ids the writer printed, handed back on the command line as bare
    // integers. `ReceiptId(pub u64)` makes this a cast, not a codec.
    let carried: Vec<nmp::ReceiptId> = args
        .iter()
        .skip(3)
        .filter_map(|value| value.parse::<u64>().ok())
        .map(nmp::ReceiptId)
        .collect();
    let app = match nmp_canary::process::durable(&store) {
        Ok(app) => app,
        Err(error) => {
            println!("child.reopen=failed:{error}");
            std::process::exit(1);
        }
    };
    println!("child.reopen=ok");
    println!("child.pid={}", std::process::id());

    for receipt in &carried {
        let verdict = match app.engine.reattach_receipt(*receipt) {
            Ok(nmp::ReceiptReattachment::Attached { .. }) => "attached",
            Ok(nmp::ReceiptReattachment::NotFound) => "not-found",
            Ok(nmp::ReceiptReattachment::RetainedButUnreadable) => "unreadable",
            Err(_) => "engine-closed",
        };
        println!("child.carried_receipt id={} reattach={verdict}", receipt.0);
    }

    let retained = nmp_canary::process::survey_publish_queue(&app.engine);
    println!("child.retained_obligations={}", retained.len());
    for entry in &retained {
        println!(
            "child.obligation event={} signing={:?} route_complete={} intended={} outcome={:?} reattach={}",
            &entry.event.to_hex()[..12],
            entry.signing,
            entry.route_complete,
            entry.intended,
            entry.outcome,
            entry.reattached
        );
    }

    // And the rows themselves, from the durable store with no prior state.
    let author = retained.first().map(|entry| entry.author);
    if let Some(author) = author {
        let query = nmp_canary::profiles::profiles_of_authors([author]);
        let _ = query; // profiles are not the point here
        let mine = nmp::LiveQuery::single(nmp::Demand {
            selection: nmp::Filter {
                authors: Some(nmp::Binding::Literal(std::collections::BTreeSet::from([
                    author.to_hex(),
                ]))),
                ..nmp::Filter::default()
            },
            ..nmp::Demand::default()
        });
        if let Ok(subscription) = app.engine.observe(mine, None) {
            let mut table = nmp_canary::rows::RowTable::new();
            for _ in 0..6 {
                match subscription.recv_timeout(Duration::from_millis(300)) {
                    Ok(frame) => table.apply(&frame),
                    Err(_) => break,
                }
            }
            println!("child.rows_from_cold_store={}", table.len());
            let signed = table
                .rows()
                .filter(|row| matches!(row.signature(), nmp::RowSignature::Signed(_)))
                .count();
            println!("child.rows_signed={signed}");
        }
    }
    println!("child.survey={}", nmp_canary::process::Survey::take());
    let elapsed = nmp_canary::process::timed_shutdown(&app.engine);
    println!("child.shutdown_returned_in={elapsed:?}");
}

fn child_contend(args: &[String]) {
    let store = args.get(2).cloned().unwrap_or_default();
    match nmp_canary::process::durable(&store) {
        Ok(app) => {
            println!("child.second_process_open=ACCEPTED");
            app.shutdown();
        }
        Err(error) => println!("child.second_process_open=refused:{error}"),
    }
    match nmp::Engine::reset_persistent_store(&store) {
        Ok(()) => println!("child.second_process_reset=ACCEPTED"),
        Err(error) => println!("child.second_process_reset=refused:{error}"),
    }
}
