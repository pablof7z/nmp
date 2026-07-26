//! Host-owned controlled relay for the Android emulator qualification (#832).
//!
//! The emulator reaches this loopback listener through Android's documented
//! `10.0.2.2` host alias. It speaks only enough NIP-01 to prove that the
//! externally packaged AAR opened a real supported-facade observation:
//! matching REQs receive one valid signed kind-1 event and EOSE. It is test
//! infrastructure, never an app-side socket bypass.

use std::io::{self, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use nostr::filter::MatchEventOptions;
use nostr::{ClientMessage, Event, EventBuilder, JsonUtil, Keys, RelayMessage};
use tungstenite::Message;

const DEFAULT_PORT: u16 = 47_391;
const EVENT_CONTENT: &str = "nmp-android-controlled-relay";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port = std::env::var("NMP_ANDROID_RELAY_PORT")
        .ok()
        .map(|value| value.parse::<u16>())
        .transpose()?
        .unwrap_or(DEFAULT_PORT);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    let event = EventBuilder::text_note(EVENT_CONTENT).sign_with_keys(&Keys::generate())?;

    println!(
        "NMP_ANDROID_RELAY_READY address={} event_id={}",
        listener.local_addr()?,
        event.id
    );
    io::stdout().flush()?;

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let event = event.clone();
                thread::spawn(move || {
                    if let Err(error) = serve_connection(stream, &event) {
                        // The engine also probes relay information over HTTP.
                        // A non-WebSocket request reaching this deliberately
                        // tiny NIP-01 server is expected and must not stop the
                        // listener that owns the actual qualification path.
                        eprintln!("NMP_ANDROID_RELAY_CONNECTION_ENDED {error}");
                    }
                });
            }
            Err(error) => eprintln!("NMP_ANDROID_RELAY_ACCEPT_ERROR {error}"),
        }
    }
    Ok(())
}

fn serve_connection(stream: TcpStream, event: &Event) -> Result<(), Box<dyn std::error::Error>> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut socket = tungstenite::accept(stream)?;

    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let Ok(ClientMessage::Req {
                    subscription_id,
                    filters,
                }) = ClientMessage::from_json(text.as_str())
                else {
                    continue;
                };
                println!(
                    "NMP_ANDROID_RELAY_REQ subscription={} filters={}",
                    subscription_id,
                    filters.len()
                );
                io::stdout().flush()?;
                if filters.into_iter().any(|filter| {
                    filter
                        .into_owned()
                        .match_event(event, MatchEventOptions::new())
                }) {
                    socket.send(Message::text(
                        RelayMessage::event(subscription_id.clone().into_owned(), event.clone())
                            .as_json(),
                    ))?;
                }
                socket.send(Message::text(
                    RelayMessage::eose(subscription_id.into_owned()).as_json(),
                ))?;
                socket.flush()?;
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(());
            }
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::UnexpectedEof
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_req_receives_valid_event_and_eose() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind relay");
        let address = listener.local_addr().expect("relay address");
        let event = EventBuilder::text_note(EVENT_CONTENT)
            .sign_with_keys(&Keys::generate())
            .expect("sign fixture event");
        let expected_id = event.id;
        let relay = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            serve_connection(stream, &event).expect("serve client");
        });

        let (mut client, _) =
            tungstenite::connect(format!("ws://{address}")).expect("connect client");
        client
            .send(Message::text(
                r#"["REQ","android-qualification",{"kinds":[1]}]"#,
            ))
            .expect("send REQ");
        let event_frame = client.read().expect("read EVENT").into_text().unwrap();
        let eose_frame = client.read().expect("read EOSE").into_text().unwrap();
        assert!(event_frame.contains("\"EVENT\""));
        assert!(event_frame.contains(expected_id.to_hex().as_str()));
        assert_eq!(eose_frame, r#"["EOSE","android-qualification"]"#);
        client.close(None).expect("close client");
        relay.join().expect("join relay");
    }
}
