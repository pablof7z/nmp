//! Host-owned controlled relay for Android runtime qualification (#832).
//!
//! Android reaches this loopback listener through the emulator's documented
//! `10.0.2.2` alias. The fixture can refuse a fixed number of WebSocket
//! handshakes before serving one deterministic signed event, which proves the
//! public facade's never-connected failure and recovery path without an app
//! socket bypass or public-network dependency.

use std::io::{self, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use nostr::filter::MatchEventOptions;
use nostr::{ClientMessage, Event, EventBuilder, JsonUtil, Keys, RelayMessage};
use tungstenite::Message;

const DEFAULT_PORT: u16 = 47_391;
const EVENT_CONTENT: &str = "nmp-android-controlled-relay";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port = env_u16("NMP_ANDROID_RELAY_PORT", DEFAULT_PORT)?;
    let fail_handshakes = env_usize("NMP_ANDROID_RELAY_FAIL_HANDSHAKES", 0)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    let event = EventBuilder::text_note(EVENT_CONTENT).sign_with_keys(&Keys::generate())?;
    let attempts = Arc::new(AtomicUsize::new(0));

    println!(
        "NMP_ANDROID_RELAY_READY address={} event_id={} fail_handshakes={fail_handshakes}",
        listener.local_addr()?,
        event.id
    );
    io::stdout().flush()?;
    if let Ok(path) = std::env::var("NMP_ANDROID_RELAY_READY_FIFO") {
        let mut ready = std::fs::OpenOptions::new().write(true).open(path)?;
        writeln!(ready, "ready")?;
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let attempts = Arc::clone(&attempts);
                let event = event.clone();
                thread::spawn(move || {
                    let mut attempt = 0;
                    let result = match is_websocket_upgrade(&stream) {
                        Ok(true) => {
                            attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                            if attempt <= fail_handshakes {
                                refuse_handshake(stream, attempt)
                            } else {
                                serve_connection(stream, &event, attempt)
                            }
                        }
                        // NMP's independent NIP-11 HTTP probe is expected to
                        // reach this tiny listener. It cannot spend one of the
                        // deterministic WebSocket-refusal attempts.
                        Ok(false) => serve_connection(stream, &event, attempt),
                        Err(error) => Err(error.into()),
                    };
                    if let Err(error) = result {
                        // The engine also probes relay information over HTTP.
                        // A non-WebSocket request is expected and cannot stop
                        // the listener that owns the actual NIP-01 proof.
                        eprintln!("NMP_ANDROID_RELAY_CONNECTION_ENDED attempt={attempt} {error}");
                    }
                });
            }
            Err(error) => eprintln!("NMP_ANDROID_RELAY_ACCEPT_ERROR {error}"),
        }
    }
    Ok(())
}

fn is_websocket_upgrade(stream: &TcpStream) -> io::Result<bool> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bytes = [0u8; 4096];
    loop {
        let read = stream.peek(&mut bytes)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before HTTP request headers",
            ));
        }
        let request = String::from_utf8_lossy(&bytes[..read]).to_ascii_lowercase();
        if request.contains("\r\n\r\n") || read == bytes.len() {
            return Ok(request.contains("upgrade: websocket"));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP request headers were incomplete",
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn env_u16(name: &str, default: u16) -> Result<u16, Box<dyn std::error::Error>> {
    Ok(std::env::var(name)
        .ok()
        .map(|value| value.parse::<u16>())
        .transpose()?
        .unwrap_or(default))
}

fn env_usize(name: &str, default: usize) -> Result<usize, Box<dyn std::error::Error>> {
    Ok(std::env::var(name)
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(default))
}

fn refuse_handshake(
    mut stream: TcpStream,
    attempt: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(
        b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    )?;
    stream.flush()?;
    println!("NMP_ANDROID_RELAY_REFUSED attempt={attempt}");
    io::stdout().flush()?;
    Ok(())
}

fn serve_connection(
    stream: TcpStream,
    event: &Event,
    attempt: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut socket = tungstenite::accept(stream)?;
    println!("NMP_ANDROID_RELAY_CONNECTED attempt={attempt}");
    io::stdout().flush()?;

    loop {
        match socket.read() {
            Ok(Message::Text(text)) => match ClientMessage::from_json(text.as_str()) {
                Ok(ClientMessage::Req {
                    subscription_id,
                    filters,
                }) => {
                    println!(
                        "NMP_ANDROID_RELAY_REQ attempt={attempt} subscription={} filters={}",
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
                            RelayMessage::event(
                                subscription_id.clone().into_owned(),
                                event.clone(),
                            )
                            .as_json(),
                        ))?;
                    }
                    socket.send(Message::text(
                        RelayMessage::eose(subscription_id.into_owned()).as_json(),
                    ))?;
                    socket.flush()?;
                    println!("NMP_ANDROID_RELAY_EOSE attempt={attempt}");
                    io::stdout().flush()?;
                }
                Ok(ClientMessage::Close(subscription_id)) => {
                    println!(
                        "NMP_ANDROID_RELAY_CLOSE attempt={attempt} subscription={subscription_id}"
                    );
                    io::stdout().flush()?;
                    return Ok(());
                }
                Ok(_) | Err(_) => {}
            },
            Ok(Message::Close(_)) => {
                println!("NMP_ANDROID_RELAY_DISCONNECT attempt={attempt}");
                io::stdout().flush()?;
                return Ok(());
            }
            Ok(_) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                println!("NMP_ANDROID_RELAY_DISCONNECT attempt={attempt}");
                io::stdout().flush()?;
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
                println!("NMP_ANDROID_RELAY_DISCONNECT attempt={attempt} io={error}");
                io::stdout().flush()?;
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
    fn matching_req_receives_valid_event_eose_and_close() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind relay");
        let address = listener.local_addr().expect("relay address");
        let event = EventBuilder::text_note(EVENT_CONTENT)
            .sign_with_keys(&Keys::generate())
            .expect("sign fixture event");
        let expected_id = event.id;
        let relay = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            serve_connection(stream, &event, 1).expect("serve client");
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
        client
            .send(Message::text(r#"["CLOSE","android-qualification"]"#))
            .expect("send CLOSE");
        relay.join().expect("join relay");
    }

    #[test]
    fn configured_handshake_refusal_is_causal() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind relay");
        let address = listener.local_addr().expect("relay address");
        let relay = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            refuse_handshake(stream, 1).expect("refuse handshake");
        });

        let failure = tungstenite::connect(format!("ws://{address}"))
            .expect_err("503 handshake unexpectedly succeeded");
        assert!(failure.to_string().contains("503"));
        relay.join().expect("join relay");
    }
}
