#![cfg(feature = "media")]

//! #971 acceptance proof: `NmpEngine::upload_blossom` performs the REAL
//! one-shot prepare -> sign -> validate -> upload sequence, against a real
//! scripted loopback HTTP server, entirely from the FFI surface -- no
//! author, timestamp, expiration, draft, sign request, or authorization
//! crosses this boundary.

mod support;

use nmp_asset::Sha256Hash;
use nmp_ffi::facade::{NmpEngine, NmpEngineConfig};
use nmp_ffi::media::FfiUploadBlossomError;
use nmp_ffi::session::FfiPrivateKey;

use support::{MockServer, ScriptedResponse};

const TEST_SECRET_KEY_HEX: &str =
    "0000000000000000000000000000000000000000000000000000000000000001";

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_blossom_performs_the_real_one_shot_sequence() {
    let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine builds");
    let keys = nostr::Keys::parse(TEST_SECRET_KEY_HEX).unwrap();
    engine
        .add_private_key_account(
            FfiPrivateKey::from_bytes(keys.secret_key().to_secret_bytes().to_vec()).unwrap(),
            true,
        )
        .expect("activate account");

    let blob = b"nmp-ffi upload_blossom reachability fixture".to_vec();
    let url = format!("https://cdn.example.com/{}", Sha256Hash::of(&blob).to_hex());
    let mock = MockServer::serve_one(ok_response(descriptor_body(&blob, &url, "image/png")));
    let server_url = mock.base_url.clone();

    let descriptor = engine
        .upload_blossom(
            server_url,
            blob.clone(),
            "image/png".to_string(),
            "upload_blossom reachability fixture".to_string(),
        )
        .await
        .expect("upload_blossom performs the real sequence and succeeds");

    let requests = mock.join();
    assert_eq!(requests.len(), 1, "exactly one PUT /upload was observed");
    assert_eq!(requests[0].method, "PUT");
    assert_eq!(requests[0].path, "/upload");
    assert_eq!(
        requests[0].body, blob,
        "the exact prepared bytes were uploaded verbatim"
    );

    assert_eq!(descriptor.url, url);
    assert_eq!(descriptor.sha256, Sha256Hash::of(&blob).to_hex());
    assert_eq!(descriptor.mime_type.as_deref(), Some("image/png"));

    engine.session().expect("engine is still open");
}

/// Falsifier: no current account is a typed, pre-signing refusal -- the
/// call never reaches the network.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_out_upload_is_refused_before_any_network_call() {
    let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine builds");
    let err = engine
        .upload_blossom(
            "https://cdn.example.com".to_string(),
            b"bytes".to_vec(),
            "image/png".to_string(),
            "no account selected".to_string(),
        )
        .await
        .expect_err("a signed-out engine must refuse before any I/O");
    assert_eq!(err, FfiUploadBlossomError::SignedOut);
}

/// Falsifier: an empty content type is refused before any signing or I/O
/// (NIP-68 imeta requires `m`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_content_type_is_refused_before_any_network_call() {
    let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine builds");
    let keys = nostr::Keys::parse(TEST_SECRET_KEY_HEX).unwrap();
    engine
        .add_private_key_account(
            FfiPrivateKey::from_bytes(keys.secret_key().to_secret_bytes().to_vec()).unwrap(),
            true,
        )
        .expect("activate account");

    let err = engine
        .upload_blossom(
            "https://cdn.example.com".to_string(),
            b"bytes".to_vec(),
            String::new(),
            "empty mime".to_string(),
        )
        .await
        .expect_err("an empty content type must be refused");
    assert_eq!(err, FfiUploadBlossomError::EmptyContentType);
}
