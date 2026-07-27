//! Live probe: what a widening subscription actually costs a relay, versus
//! asking only for the values it has not asked for yet (#933).
//!
//! This measures the RELAY's side of the trade, so it speaks NIP-01 directly
//! rather than going through `Engine`. What NMP puts on the socket when a
//! value set grows is already measured elsewhere
//! (`crates/nmp/examples/tag_fanout_live.rs`): one subscription id, one
//! overwriting REQ per growth step, carrying the cumulative value set. This
//! probe takes that as given and asks the next question — how many events
//! does the relay re-send because of it, and how many bytes is that.
//!
//! ```text
//! nak serve --port 10547
//! # seed: N events per #p value (see the module docs in the study)
//! cargo run -p nmp --example reserve_cost_live -- ws://localhost:10547 5,1,3
//! ```
//!
//! The second argument is the GROWTH SCHEDULE: how many new values each step
//! reveals. `5,1,3` is the worked example from #933 — a cache resolves five,
//! one relay's EOSE reveals a sixth, another reveals three more.
//!
//! Two strategies are run against the same seeded relay, back to back:
//!
//! - `overwrite` — today's behaviour. One subscription id; each step sends a
//!   REQ carrying every value discovered so far.
//! - `delta` — #933's proposal. The first subscription stays open untouched;
//!   each later step opens a SEPARATE subscription carrying only that step's
//!   new values.
//!
//! Both end holding the same demand. The difference in EVENT frames is the
//! whole of what #933 would save, and the difference in concurrent
//! subscriptions is what it would cost against the ~20-subscription relay
//! ceiling `nmp_router::CompileBudget` enforces.

use std::collections::BTreeSet;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use tungstenite::{connect, Message, WebSocket};

type Socket = WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

fn hex32(i: usize) -> String {
    format!("{i:064x}")
}

/// `["REQ", <id>, {"kinds":[1],"#p":[...]}]`, hand-built so the probe owns
/// exactly what goes on the wire.
fn req_frame(sub_id: &str, values: &BTreeSet<usize>) -> String {
    let list = values
        .iter()
        .map(|v| format!("\"{}\"", hex32(*v)))
        .collect::<Vec<_>>()
        .join(",");
    format!("[\"REQ\",\"{sub_id}\",{{\"kinds\":[1],\"#p\":[{list}]}}]")
}

/// One step's tally.
#[derive(Default, Clone, Copy)]
struct Tally {
    events: usize,
    bytes: usize,
}

/// Send `frame`, then read until this subscription's EOSE, counting the
/// EVENT frames the relay serves under `sub_id`.
fn drain_to_eose(socket: &mut Socket, sub_id: &str, frame: &str) -> Tally {
    socket
        .send(Message::Text(frame.to_string().into()))
        .expect("send REQ");

    let mut tally = Tally::default();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if Instant::now() > deadline {
            eprintln!("  ! timed out waiting for EOSE on {sub_id}");
            return tally;
        }
        let text = match socket.read().expect("read frame") {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => panic!("relay closed the connection"),
            _ => continue,
        };
        if text.starts_with(&format!("[\"EVENT\",\"{sub_id}\"")) {
            tally.events += 1;
            tally.bytes += text.len();
        } else if text.starts_with(&format!("[\"EOSE\",\"{sub_id}\"")) {
            return tally;
        }
    }
}

fn open(url: &str) -> Socket {
    let (socket, _) = connect(url).expect("connect to relay");
    socket
}

fn main() {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "ws://localhost:10547".into());
    let schedule: Vec<usize> = args
        .next()
        .unwrap_or_else(|| "5,1,3".into())
        .split(',')
        .map(|s| s.trim().parse().expect("growth schedule is a,b,c"))
        .collect();

    let total_values: usize = schedule.iter().sum();
    println!("relay    : {url}");
    println!(
        "schedule : {schedule:?}  ({total_values} values over {} steps)",
        schedule.len()
    );
    println!();

    // --- strategy: overwrite in place (today) ---------------------------
    let mut socket = open(&url);
    let mut cumulative: BTreeSet<usize> = BTreeSet::new();
    let mut next_value = 0usize;
    let mut overwrite = Tally::default();
    println!("overwrite (one sub id, cumulative filter):");
    for (step, count) in schedule.iter().enumerate() {
        for _ in 0..*count {
            cumulative.insert(next_value);
            next_value += 1;
        }
        let frame = req_frame("overwrite", &cumulative);
        let t = drain_to_eose(&mut socket, "overwrite", &frame);
        println!(
            "  step {step}: {:>3} values -> {:>5} events, {:>8} bytes",
            cumulative.len(),
            t.events,
            t.bytes
        );
        overwrite.events += t.events;
        overwrite.bytes += t.bytes;
    }
    let overwrite_subs = 1;
    drop(socket);

    // --- strategy: per-EOSE delta (#933's proposal) ---------------------
    let mut socket = open(&url);
    let mut next_value = 0usize;
    let mut delta = Tally::default();
    println!();
    println!("delta (incumbent untouched, one sub per growth step):");
    for (step, count) in schedule.iter().enumerate() {
        let mut fresh: BTreeSet<usize> = BTreeSet::new();
        for _ in 0..*count {
            fresh.insert(next_value);
            next_value += 1;
        }
        let sub_id = format!("delta{step}");
        let frame = req_frame(&sub_id, &fresh);
        let t = drain_to_eose(&mut socket, &sub_id, &frame);
        println!(
            "  step {step}: {:>3} values -> {:>5} events, {:>8} bytes",
            fresh.len(),
            t.events,
            t.bytes
        );
        delta.events += t.events;
        delta.bytes += t.bytes;
    }
    let delta_subs = schedule.len();

    println!();
    println!("                     events      bytes   concurrent subs");
    println!(
        "overwrite      {:>12} {:>10} {:>17}",
        overwrite.events, overwrite.bytes, overwrite_subs
    );
    println!(
        "delta          {:>12} {:>10} {:>17}",
        delta.events, delta.bytes, delta_subs
    );
    let saved_events = overwrite.events.saturating_sub(delta.events);
    let saved_bytes = overwrite.bytes.saturating_sub(delta.bytes);
    let pct = if overwrite.bytes == 0 {
        0.0
    } else {
        100.0 * saved_bytes as f64 / overwrite.bytes as f64
    };
    println!(
        "saved          {:>12} {:>10} {:>16}",
        saved_events,
        saved_bytes,
        format!("-{}", delta_subs - overwrite_subs)
    );
    println!(
        "saving: {pct:.1}% of served bytes, at {}x the subscriptions",
        delta_subs
    );
}
