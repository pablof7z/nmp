//! #971: the direct/FFI oracle for the one-shot engine-authorized Blossom
//! upload.
//!
//! The whole point of the operation is that NMP, not the caller, decides what
//! goes on the wire. So the parity claim is about the WIRE, not about the
//! return value alone: the same four product inputs, driven through the
//! supported direct Rust facade and through `nmp-ffi`, must produce the same
//! request method and path, the same body bytes, the same `Content-Type` and
//! `X-SHA-256`, and the same BUD-11 authorization event apart from the two
//! values that legitimately differ between two runs -- the instant each engine
//! read from its own clock, and the schnorr nonce.
//!
//! Each side gets its own instance of the SAME local server; there is no
//! second Blossom fake here and no mock of either facade.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use nmp::{BlossomUploadError, BlossomUploadRequest, Engine, EngineConfig};
use nmp_ffi::blossom::{FfiBlossomUploadFailure, FfiBlossomUploadRequest};
use nmp_ffi::facade::{NmpEngine, NmpEngineConfig};
use nostr::{Event, JsonUtil, Tag};

const SECRET_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000071";
const CONTENT_TYPE: &str = "application/pdf";
const DESCRIPTION: &str = "Upload the signed report";

/// What one Blossom server actually received, reduced to the facts a parity
/// claim can be made about.
#[derive(Debug, PartialEq, Eq)]
struct ObservedRequest {
    request_line: String,
    content_type: Option<String>,
    x_sha_256: Option<String>,
    body: Vec<u8>,
    author: String,
    kind: u16,
    tags: Vec<Vec<String>>,
    content: String,
    /// `expiration - created_at`: the governed authorization lifetime. The two
    /// absolute timestamps differ between runs; the WINDOW must not.
    authorization_lifetime_secs: u64,
}

/// The descriptor each facade handed back, reduced the same way.
#[derive(Debug, PartialEq, Eq)]
struct ObservedDescriptor {
    url: String,
    sha256: String,
    size: u64,
    mime_type: Option<String>,
}

/// The fixture deliberately does NOT join its accept thread on drop: an
/// assertion that fires before the upload reaches the socket would otherwise
/// turn a readable test failure into a hang on a listener nobody will ever
/// connect to.
struct TestServer {
    url: String,
    captured: mpsc::Receiver<(String, Vec<u8>)>,
}

fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let (header_end, content_length) = loop {
        let read = stream.read(&mut chunk).expect("request read");
        assert!(read > 0, "client closed before a complete request");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(marker) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = marker + 4;
            let head = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            break (header_end, content_length);
        }
    };
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut chunk).expect("request body read");
        assert!(read > 0, "client closed before a complete body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    (
        String::from_utf8(bytes[..header_end].to_vec()).unwrap(),
        bytes[header_end..header_end + content_length].to_vec(),
    )
}

fn spawn_server(descriptor: String) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (captured_tx, captured_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("upload connection");
        let request = read_request(&mut stream);
        captured_tx.send(request).unwrap();
        let body = descriptor.into_bytes();
        let head = format!(
            "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
    });
    TestServer {
        url: format!("http://{address}"),
        captured: captured_rx,
    }
}

fn blob() -> Vec<u8> {
    let mut bytes = b"%PDF parity bytes\r\n".to_vec();
    bytes.extend_from_slice(&[0x00, 0xff, 0x7f, 0x80]);
    bytes
}

/// The harness's own digest, computed through `nostr`'s bundled hashing rather
/// than through either facade, so neither side can define the truth it is
/// being measured against.
fn sha256_hex(bytes: &[u8]) -> String {
    use nostr::hashes::{sha256::Hash as Sha256, Hash as _};
    use std::fmt::Write as _;
    Sha256::hash(bytes)
        .to_byte_array()
        .iter()
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn descriptor_json(hash: &str, size: usize) -> String {
    format!(
        r#"{{"url":"https://cdn.example/{hash}","sha256":"{hash}","size":{size},"type":"{CONTENT_TYPE}"}}"#
    )
}

fn header(head: &str, wanted: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case(wanted)
            .then(|| value.trim().to_string())
    })
}

fn observe(head: &str, body: Vec<u8>) -> ObservedRequest {
    let request_line = head.lines().next().expect("a request line").to_string();
    let authorization = header(head, "authorization").expect("a BUD-11 header");
    let encoded = authorization
        .strip_prefix("Nostr ")
        .expect("the BUD-11 header prefix");
    let json = base64_url_decode(encoded);
    let event = Event::from_json(json).expect("a canonical signed event");
    let tags: Vec<Vec<String>> = event.tags.iter().cloned().map(Tag::to_vec).collect();
    let expiration: u64 = tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some("expiration"))
        .and_then(|tag| tag.get(1))
        .expect("BUD-11 mandates an expiration")
        .parse()
        .expect("a numeric expiration");
    // Everything but the two values that legitimately differ per run.
    let stable_tags = tags
        .into_iter()
        .filter(|tag| tag.first().map(String::as_str) != Some("expiration"))
        .collect();
    ObservedRequest {
        request_line,
        content_type: header(head, "content-type"),
        x_sha_256: header(head, "x-sha-256"),
        body,
        author: event.pubkey.to_hex(),
        kind: event.kind.as_u16(),
        tags: stable_tags,
        content: event.content.clone(),
        authorization_lifetime_secs: expiration - event.created_at.as_secs(),
    }
}

fn base64_url_decode(encoded: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let index: BTreeMap<u8, u32> = ALPHABET
        .iter()
        .enumerate()
        .map(|(position, byte)| (*byte, position as u32))
        .collect();
    let mut out = Vec::new();
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in encoded.bytes().filter(|byte| *byte != b'=') {
        let value = index
            .get(&byte)
            .unwrap_or_else(|| panic!("BUD-11 uses base64url, saw {byte:?}"));
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    out
}

fn direct_upload(server_url: String, bytes: Vec<u8>) -> (ObservedDescriptor, String) {
    let engine = Engine::new(EngineConfig {
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..EngineConfig::default()
    })
    .expect("direct engine must build");
    let author = engine
        .add_account(SECRET_KEY)
        .expect("account must register")
        .public_key();
    engine
        .set_active_account(Some(author))
        .expect("account must activate");
    let uploaded = engine
        .upload_blossom(BlossomUploadRequest {
            server_url,
            bytes,
            content_type: CONTENT_TYPE.to_string(),
            description: DESCRIPTION.to_string(),
        })
        .expect("the direct upload must start")
        .recv()
        .expect("the direct upload must succeed");
    let descriptor = uploaded.descriptor();
    let observed = ObservedDescriptor {
        url: descriptor.url.clone(),
        sha256: descriptor.sha256.to_hex(),
        size: descriptor.size,
        mime_type: descriptor.mime_type.clone(),
    };
    engine.shutdown();
    (observed, author.to_hex())
}

async fn ffi_upload(server_url: String, blob: Vec<u8>) -> (ObservedDescriptor, String) {
    let engine = NmpEngine::new(NmpEngineConfig {
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..NmpEngineConfig::default()
    })
    .expect("ffi engine must build");
    let author = engine
        .add_account(SECRET_KEY.to_string())
        .expect("account must register")
        .public_key();
    engine
        .set_active_account(Some(author.clone()))
        .expect("account must activate");
    let descriptor = engine
        .upload_blossom(FfiBlossomUploadRequest {
            server_url,
            blob,
            content_type: CONTENT_TYPE.to_string(),
            description: DESCRIPTION.to_string(),
        })
        .expect("the ffi upload must start")
        .uploaded()
        .await
        .expect("the ffi upload must succeed");
    let observed = ObservedDescriptor {
        url: descriptor.url,
        sha256: descriptor.sha256,
        size: descriptor.size,
        mime_type: descriptor.mime_type,
    };
    engine.shutdown();
    (observed, author)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_and_public_ffi_engine_authorized_uploads_put_the_same_thing_on_the_wire() {
    let bytes = blob();
    let hash = sha256_hex(&bytes);
    let descriptor = descriptor_json(&hash, bytes.len());

    let direct_server = spawn_server(descriptor.clone());
    let (direct_descriptor, direct_author) =
        direct_upload(direct_server.url.clone(), bytes.clone());
    let (direct_head, direct_body) = direct_server
        .captured
        .recv_timeout(Duration::from_secs(10))
        .expect("the direct facade must reach the server");

    let ffi_server = spawn_server(descriptor);
    let (ffi_descriptor, ffi_author) = ffi_upload(ffi_server.url.clone(), bytes.clone()).await;
    let (ffi_head, ffi_body) = ffi_server
        .captured
        .recv_timeout(Duration::from_secs(10))
        .expect("the ffi facade must reach the server");

    let direct = observe(&direct_head, direct_body);
    let ffi = observe(&ffi_head, ffi_body);

    assert_eq!(
        direct, ffi,
        "the two supported spellings must put the same request on the wire"
    );
    assert_eq!(direct_descriptor, ffi_descriptor);
    assert_eq!(direct_author, ffi_author);

    // Not a tautology check: pin the actual values, so a change that made BOTH
    // sides wrong in the same way would still fail here.
    assert!(direct.request_line.starts_with("PUT /upload "));
    assert_eq!(direct.body, bytes);
    assert_eq!(direct.content_type.as_deref(), Some(CONTENT_TYPE));
    assert_eq!(direct.x_sha_256.as_deref(), Some(hash.as_str()));
    assert_eq!(
        direct.author, direct_author,
        "the BUD-11 author is the active account, not something the caller named"
    );
    assert_eq!(direct.kind, 24_242);
    assert_eq!(direct.content, DESCRIPTION);
    assert!(direct
        .tags
        .contains(&vec!["t".to_string(), "upload".to_string()]));
    assert!(direct.tags.contains(&vec!["x".to_string(), hash.clone()]));
    assert_eq!(direct_descriptor.sha256, hash);
    assert_eq!(direct_descriptor.size, bytes.len() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_and_public_ffi_refuse_the_same_upload_for_the_same_typed_reason() {
    let direct = Engine::new(EngineConfig::default()).expect("direct engine must build");
    let ffi = NmpEngine::new(NmpEngineConfig::default()).expect("ffi engine must build");
    let request = |server_url: &str| BlossomUploadRequest {
        server_url: server_url.to_string(),
        bytes: b"refused".to_vec(),
        content_type: CONTENT_TYPE.to_string(),
        description: DESCRIPTION.to_string(),
    };
    let ffi_request = |server_url: &str| FfiBlossomUploadRequest {
        server_url: server_url.to_string(),
        blob: b"refused".to_vec(),
        content_type: CONTENT_TYPE.to_string(),
        description: DESCRIPTION.to_string(),
    };

    // Refused for the URL before anything else, on both sides.
    assert!(matches!(
        direct.upload_blossom(request("ftp://blobs.example")).err(),
        Some(BlossomUploadError::InvalidServerUrl(_))
    ));
    assert!(matches!(
        ffi.upload_blossom(ffi_request("ftp://blobs.example")).err(),
        Some(FfiBlossomUploadFailure::InvalidServerUrl { .. })
    ));

    // Refused for the signer, on both sides.
    assert_eq!(
        direct
            .upload_blossom(request("https://blobs.example"))
            .err(),
        Some(BlossomUploadError::NoActiveSigner)
    );
    assert_eq!(
        ffi.upload_blossom(ffi_request("https://blobs.example"))
            .err(),
        Some(FfiBlossomUploadFailure::NoActiveSigner)
    );

    direct.shutdown();
    ffi.shutdown();
}
