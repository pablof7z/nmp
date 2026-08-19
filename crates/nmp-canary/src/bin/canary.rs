//! Drives `nmp-canary` against a real `nmp::Engine` with a local store and no
//! reachable relay. This is the proof that the library above is not merely
//! type-correct: every surface is opened, written to, and read back.
//!
//! No relay means every write settles as `NoDestination` and every read is
//! served from the canonical local store. That is enough to exercise the
//! SURFACE, which is what this crate exists to measure. The relay half is a
//! separate harness.
//!
//! Run: `cargo run -p nmp-canary --bin canary`

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

fn main() {
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

    println!("\n{}", "=".repeat(72));
    print!("{}", nmp_canary::report());
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
