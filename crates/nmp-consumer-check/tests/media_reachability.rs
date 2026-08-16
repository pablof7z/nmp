//! #1563 acceptance proof: the composed media path (`nmp::media`,
//! `nmp::nip68`, `nmp::blossom`, `nmp::asset`) is reachable AND USABLE from
//! `nmp` alone -- a compile plus a real run, not an argument. Before this
//! PR, `nmp-media` had no facade feature at all: roughly 1,600 lines of
//! media composition (this crate plus `nmp-nip68`) were unreachable from any
//! app in any language.
//!
//! Drives the real three-stage seam end to end: `prepare` -> sign the BUD-11
//! authorization draft through the engine's own `sign_event` (never a second
//! signer) -> `upload` against a real scripted loopback HTTP server -> the
//! final `compose_picture`. This crate's own manifest still names `nmp`
//! alone; the mock server is pure `std::net`, and the async upload stage
//! runs on the engine's own `adapter_runtime()` handle -- no `tokio`
//! dependency of this crate's own.

mod support;

use nmp::asset::Sha256Hash;
use nmp::blossom::{
    BlossomClient, BlossomClientConfig, BlossomServerUrl, BlossomVerb, ExpectedAuthorization,
    SignedAuthorization,
};
use nmp::media::{compose_picture, prepare, ComposedImage, PicturePost};
use nmp::{Engine, EngineConfig, Kind, PublicKey, SignEventRequest, Tag, Timestamp};
use support::{MockServer, ScriptedResponse};

/// A fixed, valid secp256k1 secret key (same fixture as the rest of this
/// crate's tests): generated once via `openssl rand -hex 32`.
const TEST_SECRET_KEY_BYTES: [u8; 32] = [
    50, 246, 223, 115, 234, 216, 80, 182, 225, 60, 6, 73, 132, 107, 122, 29, 150, 70, 214, 160,
    181, 12, 105, 54, 25, 129, 23, 110, 129, 126, 112, 248,
];

fn descriptor_body(blob: &[u8], url: &str, mime: &str) -> Vec<u8> {
    let hex = Sha256Hash::of(blob).to_hex();
    format!(
        r#"{{"url":"{url}","sha256":"{hex}","size":{},"type":"{mime}"}}"#,
        blob.len()
    )
    .into_bytes()
}

fn ok_response(body: Vec<u8>) -> ScriptedResponse {
    ScriptedResponse {
        status_line: "HTTP/1.1 200 OK",
        extra_headers: vec![("Content-Type", "application/json".to_string())],
        body,
    }
}

fn tag_name(tag: &Tag) -> &str {
    tag.as_slice().first().map(String::as_str).unwrap_or("")
}

#[test]
fn composed_media_path_is_usable_from_nmp_alone() {
    let engine = Engine::new(EngineConfig::default()).expect("temporary Redb engine must build");
    let account = engine
        .add_private_key_account(&TEST_SECRET_KEY_BYTES, true)
        .expect("fixed decoded test secret key must validate");
    let author: PublicKey = engine
        .session()
        .expect("engine is open")
        .current_pubkey
        .expect("the account just added is current");

    let now = Timestamp::now();
    let past = Timestamp::from(now.as_secs() - 5);
    let future = Timestamp::from(now.as_secs() + 600);
    let blob = b"nmp-consumer-check media reachability fixture";

    // Stage 1: prepare -- pure Rust, no I/O, exact-bytes/hash binding.
    let prepared = prepare(
        blob.to_vec(),
        "image/png",
        author,
        past,
        future,
        "reachability fixture upload",
    )
    .expect("prepare succeeds from nmp alone");

    // Sign the BUD-11 authorization draft through the engine's OWN signer --
    // this crate never holds a key or a second signing path.
    let draft = prepared.authorization_draft();
    let request = SignEventRequest {
        created_at: draft.created_at,
        kind: draft.kind,
        tags: draft.tags.clone().to_vec(),
        content: draft.content.clone(),
    };
    let signed = engine
        .sign_event(request)
        .expect("sign_event is reachable from nmp alone")
        .recv()
        .expect("the engine's registered signer signs the authorization draft");
    let auth = SignedAuthorization::validate(
        signed,
        &ExpectedAuthorization {
            verb: BlossomVerb::Upload,
            blob: Some(prepared.sha256()),
        },
        now,
    )
    .expect("freshly signed authorization validates");

    // Stage 2: upload -- a REAL async HTTP round trip against a scripted
    // loopback server, driven on the engine's own tokio handle (no `tokio`
    // dependency of this crate's own).
    let url = format!("https://cdn.example.com/{}", Sha256Hash::of(blob).to_hex());
    let mock = MockServer::serve_one(ok_response(descriptor_body(blob, &url, "image/png")));
    let server = BlossomServerUrl::parse(&mock.base_url).expect("mock server url parses");
    let client = BlossomClient::new(BlossomClientConfig::default()).expect("client construction");
    let runtime = engine
        .adapter_runtime()
        .expect("the engine's own runtime handle is reachable from nmp alone");
    let asset = runtime
        .block_on(prepared.upload(&client, &server, &auth))
        .expect("upload succeeds against the real mock server");
    let requests = mock.join();
    assert_eq!(requests.len(), 1, "exactly one PUT /upload was observed");
    assert_eq!(
        requests[0].body, blob,
        "the held bytes were uploaded verbatim"
    );

    // Stage 3: compose -- the final unsigned kind:20 draft.
    let post = PicturePost {
        title: Some("nmp-consumer-check reachability".to_string()),
        description: "proving the composed media path from nmp alone".to_string(),
        content_warning: None,
        hashtags: vec!["nmp".to_string()],
    };
    let event = compose_picture(author, now, vec![ComposedImage::new(asset)], &post)
        .expect("compose_picture is reachable and usable from nmp alone");
    assert_eq!(event.kind, Kind::from(20u16));
    let imeta = event
        .tags
        .iter()
        .find(|tag| tag_name(tag) == "imeta")
        .expect("the composed draft carries an imeta row");
    assert!(imeta
        .as_slice()
        .iter()
        .any(|value| value == &format!("x {}", Sha256Hash::of(blob).to_hex())));

    assert!(engine
        .remove_account(&account)
        .expect("remove_account must be reachable from nmp alone"));
    engine.shutdown();
}
