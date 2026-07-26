//! Host-owned controlled relay for Android emulator qualification (#832/#834).
//!
//! The emulator reaches this loopback listener through Android's documented
//! `10.0.2.2` host alias. It provides four bounded test seams over ordinary
//! Nostr relay frames: one valid kind-1 row, the NIP-65 write route for the
//! bunker-controlled user, a multi-connection NIP-46 bunker, and ACKs for
//! valid writes. It is test infrastructure, never app-side relay or signer
//! machinery.

use std::io::{self, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use nostr::filter::MatchEventOptions;
use nostr::nips::nip44;
use nostr::{
    ClientMessage, Event, EventBuilder, JsonUtil, Keys, Kind, PublicKey, RelayMessage, Tag, Tags,
    Timestamp, UnsignedEvent,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tungstenite::Message;

const DEFAULT_PORT: u16 = 47_391;
const EVENT_CONTENT: &str = "nmp-android-controlled-relay";

#[derive(Clone)]
struct RelayFixture {
    row: Event,
    relay_list: Event,
    remote_signer: Keys,
    user: Keys,
    pairing_secret: String,
}

#[derive(Deserialize)]
struct SignBody {
    kind: u16,
    created_at: u64,
    tags: Vec<Vec<String>>,
    content: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port = std::env::var("NMP_ANDROID_RELAY_PORT")
        .ok()
        .map(|value| value.parse::<u16>())
        .transpose()?
        .unwrap_or(DEFAULT_PORT);
    let pairing_secret = std::env::var("NMP_ANDROID_NIP46_SECRET").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "NMP_ANDROID_NIP46_SECRET is required",
        )
    })?;
    if pairing_secret.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NMP_ANDROID_NIP46_SECRET must not be empty",
        )
        .into());
    }

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    let user = Keys::generate();
    let emulator_relay = nostr::RelayUrl::parse(&format!("ws://10.0.2.2:{port}"))?;
    let fixture = Arc::new(RelayFixture {
        row: EventBuilder::text_note(EVENT_CONTENT).sign_with_keys(&Keys::generate())?,
        remote_signer: Keys::generate(),
        relay_list: EventBuilder::new(Kind::RelayList, "")
            .tags(Tags::from_list(vec![Tag::relay_metadata(
                emulator_relay,
                None,
            )]))
            .sign_with_keys(&user)?,
        user,
        pairing_secret,
    });

    println!(
        "NMP_ANDROID_RELAY_READY address={} event_id={} remote_signer={} user={}",
        listener.local_addr()?,
        fixture.row.id,
        fixture.remote_signer.public_key(),
        fixture.user.public_key(),
    );
    io::stdout().flush()?;

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let fixture = Arc::clone(&fixture);
                thread::spawn(move || {
                    if let Err(error) = serve_connection(stream, &fixture) {
                        // The engine also probes relay information over HTTP.
                        // A non-WebSocket request reaching this deliberately
                        // small server must not stop the listener.
                        eprintln!("NMP_ANDROID_RELAY_CONNECTION_ENDED {error}");
                    }
                });
            }
            Err(error) => eprintln!("NMP_ANDROID_RELAY_ACCEPT_ERROR {error}"),
        }
    }
    Ok(())
}

fn serve_connection(
    stream: TcpStream,
    fixture: &RelayFixture,
) -> Result<(), Box<dyn std::error::Error>> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut socket = tungstenite::accept(stream)?;
    let mut subscription_id = None;

    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let Ok(message) = ClientMessage::from_json(text.as_str()) else {
                    continue;
                };
                match message {
                    ClientMessage::Req {
                        subscription_id: requested_id,
                        filters,
                    } => {
                        let requested_id = requested_id.into_owned();
                        subscription_id = Some(requested_id.clone());
                        println!(
                            "NMP_ANDROID_RELAY_REQ subscription={} filters={}",
                            requested_id,
                            filters.len()
                        );
                        io::stdout().flush()?;
                        for event in [&fixture.row, &fixture.relay_list] {
                            if filters.iter().any(|filter| {
                                filter
                                    .clone()
                                    .into_owned()
                                    .match_event(event, MatchEventOptions::new())
                            }) {
                                socket.send(Message::text(
                                    RelayMessage::event(requested_id.clone(), event.clone())
                                        .as_json(),
                                ))?;
                            }
                        }
                        socket.send(Message::text(RelayMessage::eose(requested_id).as_json()))?;
                        socket.flush()?;
                    }
                    ClientMessage::Event(event) => {
                        let event = event.into_owned();
                        event.verify()?;
                        if event.kind == Kind::NostrConnect {
                            let response =
                                nip46_response(fixture, &event, subscription_id.as_ref())?;
                            socket.send(Message::text(response))?;
                            socket.flush()?;
                        } else {
                            println!(
                                "NMP_ANDROID_RELAY_WRITE event_id={} kind={}",
                                event.id,
                                event.kind.as_u16(),
                            );
                            io::stdout().flush()?;
                            socket.send(Message::text(
                                RelayMessage::ok(event.id, true, "android qualification").as_json(),
                            ))?;
                            socket.flush()?;
                        }
                    }
                    ClientMessage::Close(_) => return Ok(()),
                    ClientMessage::Auth(_)
                    | ClientMessage::Count { .. }
                    | ClientMessage::NegOpen { .. }
                    | ClientMessage::NegMsg { .. }
                    | ClientMessage::NegClose { .. } => {}
                }
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

fn nip46_response(
    fixture: &RelayFixture,
    event: &Event,
    subscription_id: Option<&nostr::SubscriptionId>,
) -> Result<String, Box<dyn std::error::Error>> {
    let plaintext = nip44::decrypt(
        fixture.remote_signer.secret_key(),
        &event.pubkey,
        event.content.as_bytes(),
    )?;
    let request: Value = serde_json::from_str(&plaintext)?;
    let id = request["id"]
        .as_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "NIP-46 request missing id"))?;
    let method = request["method"].as_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "NIP-46 request missing method")
    })?;
    let params = request["params"].as_array().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "NIP-46 request missing params")
    })?;
    let result = match method {
        "connect" => {
            let supplied = params.get(1).and_then(Value::as_str).unwrap_or_default();
            if supplied != fixture.pairing_secret {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "NIP-46 pairing secret mismatch",
                )
                .into());
            }
            "ack".to_string()
        }
        "get_public_key" => fixture.user.public_key().to_hex(),
        "switch_relays" => "null".to_string(),
        "sign_event" => sign_event(fixture, params)?,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected NIP-46 method {other}"),
            )
            .into());
        }
    };
    println!("NMP_ANDROID_NIP46_METHOD {method}");
    io::stdout().flush()?;

    let response = response_event(&fixture.remote_signer, event.pubkey, id, result)?;
    let subscription_id = subscription_id.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "NIP-46 EVENT arrived before REQ",
        )
    })?;
    Ok(RelayMessage::event(subscription_id.clone(), response).as_json())
}

fn sign_event(
    fixture: &RelayFixture,
    params: &[Value],
) -> Result<String, Box<dyn std::error::Error>> {
    let body: SignBody =
        serde_json::from_str(params.first().and_then(Value::as_str).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "sign_event missing body")
        })?)?;
    let tags = body
        .tags
        .iter()
        .map(Tag::parse)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(UnsignedEvent::new(
        fixture.user.public_key(),
        Timestamp::from(body.created_at),
        Kind::from_u16(body.kind),
        tags,
        body.content,
    )
    .sign_with_keys(&fixture.user)?
    .as_json())
}

fn response_event(
    remote: &Keys,
    client: PublicKey,
    id: &str,
    result: String,
) -> Result<Event, Box<dyn std::error::Error>> {
    let plaintext = json!({ "id": id, "result": result, "error": null }).to_string();
    let ciphertext = nip44::encrypt(
        remote.secret_key(),
        &client,
        plaintext,
        nip44::Version::default(),
    )?;
    Ok(EventBuilder::new(Kind::NostrConnect, ciphertext)
        .tag(Tag::public_key(client))
        .sign_with_keys(remote)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> RelayFixture {
        let user = Keys::generate();
        RelayFixture {
            row: EventBuilder::text_note(EVENT_CONTENT)
                .sign_with_keys(&Keys::generate())
                .expect("sign fixture event"),
            relay_list: EventBuilder::new(Kind::RelayList, "")
                .tags(Tags::from_list(vec![Tag::relay_metadata(
                    nostr::RelayUrl::parse("ws://127.0.0.1:47391").expect("parse fixture relay"),
                    None,
                )]))
                .sign_with_keys(&user)
                .expect("sign fixture relay list"),
            remote_signer: Keys::generate(),
            user,
            pairing_secret: "test-pairing-secret".to_string(),
        }
    }

    #[test]
    fn matching_req_receives_valid_event_and_eose() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind relay");
        let address = listener.local_addr().expect("relay address");
        let fixture = fixture();
        let expected_id = fixture.row.id;
        let relay = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            serve_connection(stream, &fixture).expect("serve client");
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

    #[test]
    fn matching_relay_list_req_receives_the_remote_users_write_route() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind relay");
        let address = listener.local_addr().expect("relay address");
        let fixture = fixture();
        let expected_id = fixture.relay_list.id;
        let expected_user = fixture.user.public_key().to_hex();
        let relay = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept client");
            serve_connection(stream, &fixture).expect("serve client");
        });

        let (mut client, _) =
            tungstenite::connect(format!("ws://{address}")).expect("connect client");
        client
            .send(Message::text(format!(
                r#"["REQ","android-route",{{"kinds":[10002],"authors":["{expected_user}"]}}]"#
            )))
            .expect("send route REQ");
        let event_frame = client.read().expect("read EVENT").into_text().unwrap();
        let eose_frame = client.read().expect("read EOSE").into_text().unwrap();
        assert!(event_frame.contains("\"EVENT\""));
        assert!(event_frame.contains(expected_id.to_hex().as_str()));
        assert_eq!(eose_frame, r#"["EOSE","android-route"]"#);
        client.close(None).expect("close client");
        relay.join().expect("join relay");
    }

    #[test]
    fn nip46_response_requires_exact_pairing_secret() {
        let fixture = fixture();
        let client = Keys::generate();
        let request = json!({
            "id": "request-1",
            "method": "connect",
            "params": [
                fixture.remote_signer.public_key().to_hex(),
                fixture.pairing_secret,
                "",
                "{}"
            ]
        })
        .to_string();
        let ciphertext = nip44::encrypt(
            client.secret_key(),
            &fixture.remote_signer.public_key(),
            request,
            nip44::Version::default(),
        )
        .expect("encrypt request");
        let event = EventBuilder::new(Kind::NostrConnect, ciphertext)
            .tag(Tag::public_key(fixture.remote_signer.public_key()))
            .sign_with_keys(&client)
            .expect("sign request");
        let subscription_id = nostr::SubscriptionId::new("nip46-test");

        let response =
            nip46_response(&fixture, &event, Some(&subscription_id)).expect("NIP-46 response");
        assert!(response.contains("\"EVENT\""));
        assert!(response.contains(subscription_id.as_str()));
    }
}
